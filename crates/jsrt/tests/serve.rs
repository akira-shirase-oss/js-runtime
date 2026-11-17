//! `rt.serve` end-to-end: real TCP, real HTTP/1.1 on the wire, real child
//! process. The tests own the client socket — a raw `TcpStream` with
//! hand-written requests, so what the wire carries is what we wrote.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

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

    fn script(&self, name: &str, source: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, source).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        path
    }

    /// A loopback port for one test's server. Not from `bind(:0)`, which
    /// hands out ephemeral ports — the same range every client socket in
    /// this busy test run takes its local port from. Ports come from
    /// 20000-29999, below the ephemeral ranges of macOS (49152+) and Linux
    /// (32768+), each handed out once per run and checked free before use.
    /// A port can still be taken between this check and the server's bind
    /// (observed here, by a holder gone a moment later); `spawn_serve`
    /// notices and starts the server again.
    fn free_port() -> u16 {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let start = std::process::id() % 10_000;
        loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert!(n < 10_000, "no free test port in 20000-29999");
            let port = 20_000 + ((start + n) % 10_000) as u16;
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return port;
            }
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(JSRT)
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .unwrap_or_else(|e| panic!("spawn jsrt: {e}"))
    }

    /// Spawn a serve script and return once *its* listener is up: every
    /// serve script here prints [`READY`] right after `rt.serve` returns
    /// (the bind is synchronous), so a connection that succeeds can only
    /// reach this child — never a stranger that happens to hold the port.
    /// If the port was taken between the probe and the bind (seen on this
    /// shared machine: something briefly holds it, then it is free), the
    /// child exits with "Address already in use" and is started again.
    fn spawn_serve(&self, script: &Path, extra: &[&str]) -> ServeChild {
        let mut args = vec!["run"];
        args.push(script.to_str().unwrap_or_else(|| panic!("utf8")));
        args.extend_from_slice(extra);
        for _attempt in 0..20 {
            // stderr is piped so tests can read what the host told the
            // operator (it is small: a few lines at most).
            let mut child = Command::new(JSRT)
                .args(&args)
                .current_dir(self.dir.path())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("spawn jsrt: {e}"));
            let stdout = child
                .stdout
                .take()
                .unwrap_or_else(|| panic!("no stdout pipe"));
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                use std::io::BufRead;
                // Keep draining after the marker so the pipe never fills.
                for line in std::io::BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let _ = tx.send(line);
                }
            });
            let deadline = Instant::now() + Duration::from_secs(20);
            let ready = loop {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    break false;
                };
                match rx.recv_timeout(left) {
                    Ok(line) if line.contains(READY) => break true,
                    Ok(_) => {}
                    Err(_) => break false,
                }
            };
            if ready {
                return ServeChild(Some(child));
            }
            let _ = child.kill();
            let out = child
                .wait_with_output()
                .unwrap_or_else(|e| panic!("wait: {e}"));
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.contains("Address already in use") {
                panic!("the server never came up: {:?}, stderr: {err}", out.status);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("the test port stayed in use");
    }
}

/// What every serve script here prints once its listener is bound.
const READY: &str = "jsrt-test: listening";

/// A spawned server that is killed if the test fails before waiting for
/// it — a listener left behind would hold its port, and its backlog, for
/// the rest of the run.
struct ServeChild(Option<Child>);

impl ServeChild {
    fn pid(&self) -> u32 {
        self.0.as_ref().map_or(0, Child::id)
    }

    /// Whether the server process has exited (without reaping its output).
    fn try_wait_exited(&mut self) -> bool {
        self.0
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))))
    }

    fn wait_with_output(mut self) -> std::io::Result<Output> {
        match self.0.take() {
            Some(child) => child.wait_with_output(),
            None => Err(std::io::Error::other("already waited")),
        }
    }
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            // A failing test gets the server's side of the story.
            if let Ok(out) = child.wait_with_output() {
                if std::thread::panicking() {
                    eprintln!(
                        "server exit {:?}, stderr: {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
        }
    }
}

fn code(out: &Output) -> i32 {
    out.status
        .code()
        .unwrap_or_else(|| panic!("jsrt killed by signal"))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Connect, retrying while the listener comes up.
fn connect(port: u16) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => return s,
            Err(e) => {
                assert!(Instant::now() < deadline, "listener never came up: {e}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Send a raw request, read the whole response until close or timeout.
fn http(port: u16, request: &str) -> (u16, String, String) {
    let mut stream = connect(port);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    stream
        .write_all(request.as_bytes())
        .unwrap_or_else(|e| panic!("write: {e}"));
    let mut raw = String::new();
    let mut buf = [0u8; 8192];
    // Read until we have full headers + the content-length's body.
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.push_str(&String::from_utf8_lossy(&buf[..n]));
                if let Some(body_start) = raw.find("\r\n\r\n") {
                    let head = &raw[..body_start];
                    let mut want = None;
                    for line in head.lines() {
                        if line.len() >= 15 && line[..15].eq_ignore_ascii_case("content-length:") {
                            want = line[15..].trim().parse::<usize>().ok();
                        }
                    }
                    match want {
                        Some(w) if raw.len() >= body_start + 4 + w => break,
                        Some(_) => {}
                        None => break, // no content-length — headers only
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => panic!(
                "read: the server reset the connection after {} bytes -- did the \
                 listener close or the run time out before this request? ({e})",
                raw.len()
            ),
            Err(e) => panic!("read: {e}"),
        }
    }
    let status: u16 = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = raw
        .find("\r\n\r\n")
        .map(|i| raw[i + 4..].to_string())
        .unwrap_or_default();
    let headers = raw
        .find("\r\n")
        .map(|i| raw[..i].to_string())
        .unwrap_or_default();
    (status, headers, body)
}

#[test]
fn serve_without_a_grant_refuses() {
    let rig = Rig::new();
    let script = rig.script(
        "s.js",
        "rt.serve({host: '127.0.0.1', port: 1}, async (r) => ({body: 'x'}));\n",
    );
    let out = rig.run(&["run", script.to_str().unwrap_or_else(|| panic!("utf8"))]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("serve denied"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn serve_on_an_undeclared_address_refuses() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let manifest = rig.script(
        "m.manifest.json",
        &format!("{{ \"serve\": {{\"host\": \"127.0.0.1\", \"port\": {port}}} }}\n"),
    );
    // Grant is for `port`; the script asks for a different one.
    let script = rig.script(
        "s.js",
        &format!(
            "rt.serve({{host: '127.0.0.1', port: {}}}, async (r) => ({{body: 'x'}}));\n",
            port + 1
        ),
    );
    let out = rig.run(&[
        "run",
        script.to_str().unwrap_or_else(|| panic!("utf8")),
        "--manifest",
        manifest.to_str().unwrap_or_else(|| panic!("utf8")),
    ]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("is not the declared listener"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn serve_answers_requests_and_survives_a_handler_crash() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let manifest = rig.script(
        "m.manifest.json",
        &format!("{{ \"serve\": {{\"host\": \"127.0.0.1\", \"port\": {port}}} }}\n"),
    );
    let script = rig.script(
        "app.js",
        "const server = rt.serve({host: '127.0.0.1', port: PORT}, async (req) => {\n\
           if (req.path === '/quit') { rt.setTimeout(() => server.close(), 0); return {status: 200, body: 'bye'}; }\n\
           if (req.path === '/boom') throw new Error('handler died');\n\
           return {status: 201, headers: {'x-jsrt': 'y'}, body: req.method + ' ' + req.path};\n\
         });\n\
         rt.print('jsrt-test: listening\\n');\n"
            .replace("PORT", &port.to_string())
            .as_str(),
    );
    let child = rig.spawn_serve(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );

    // A plain GET answers method + path.
    let (status, _head, body) = http(
        port,
        &format!("GET /hello?q=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(status, 201, "body: {body}");
    assert!(body.contains("GET /hello?q=1"), "body: {body}");

    // The handler throws on /boom — 500 on the wire, and the host lives.
    let (status, _head, body) = http(
        port,
        &format!("GET /boom HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(status, 500, "body: {body}");

    // Next request still answers — the crash never reached the host.
    let (status, _head, body) = http(
        port,
        &format!("GET /alive HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(status, 201, "body: {body}");
    assert!(body.contains("GET /alive"), "body: {body}");

    // A POST body lands on the request the handler sees.
    let (status, _head, body) = http(
        port,
        &format!("POST /echo HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 4\r\nConnection: close\r\n\r\nping"),
    );
    assert_eq!(status, 201, "body: {body}");
    assert!(body.contains("POST /echo"), "body: {body}");

    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn serve_over_cap_body_is_refused_before_the_script_sees_it() {
    let rig = Rig::new();
    let port = Rig::free_port();
    // 64-byte cap; the handler counts requests — the over-cap one must
    // never arrive.
    let manifest = rig.script(
        "m.manifest.json",
        &format!(
            "{{ \"serve\": {{\"host\": \"127.0.0.1\", \"port\": {port}, \"max_body\": 64}} }}\n"
        ),
    );
    let script = rig.script(
        "app.js",
        "let seen = 0;\n\
         const server = rt.serve({host: '127.0.0.1', port: PORT}, async (req) => {\n\
           if (req.path === '/quit') { rt.setTimeout(() => server.close(), 0); return {status: 200, body: 'bye'}; }\n\
           seen++;\n\
           return {status: 200, body: 'seen:' + seen + ' len:' + req.body.length};\n\
         });\n\
         rt.print('jsrt-test: listening\\n');\n"
            .replace("PORT", &port.to_string())
            .as_str(),
    );
    let child = rig.spawn_serve(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );

    // A 4KB body against a 64B cap -> 413 from the wire layer, never
    // dispatched.
    let big = "x".repeat(4096);
    let (status, _head, _body) = http(
        port,
        &format!(
            "POST /big HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n{big}"
        ),
    );
    assert_eq!(status, 413, "over-cap body must answer 413");

    // A small body arrives and reports the count is still 1 — the
    // rejected request never reached the handler.
    let (status, _head, body) = http(
        port,
        &format!("POST /ok HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc"),
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("seen:1"), "body: {body}");

    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn serve_flag_grants_a_listener() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let script = rig.script(
        "app.js",
        "const server = rt.serve({host: '127.0.0.1', port: PORT}, async (req) => {\n\
           if (req.path === '/quit') { rt.setTimeout(() => server.close(), 0); return {status: 200, body: 'bye'}; }\n\
           return {status: 200, body: 'flag'};\n\
         });\n\
         rt.print('jsrt-test: listening\\n');\n"
            .replace("PORT", &port.to_string())
            .as_str(),
    );
    let child = rig.spawn_serve(&script, &["--serve", &format!("127.0.0.1:{port}")]);
    let (status, _head, body) = http(
        port,
        &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body, "flag");
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn a_non_loopback_serve_grant_fails_at_manifest_parse() {
    let rig = Rig::new();
    let manifest = rig.script(
        "m.manifest.json",
        "{ \"serve\": {\"host\": \"0.0.0.0\", \"port\": 80} }\n",
    );
    let script = rig.script("s.js", "rt.print('hi');\n");
    let out = rig.run(&[
        "run",
        script.to_str().unwrap_or_else(|| panic!("utf8")),
        "--manifest",
        manifest.to_str().unwrap_or_else(|| panic!("utf8")),
    ]);
    assert_eq!(code(&out), 8, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("not loopback"),
        "stderr: {}",
        stderr(&out)
    );
}

// ---- Bounds a client cannot talk its way past ------------------------------
//
// Loopback-only is not a DoS bound: anything on the box can connect. Each
// listener caps its connections, closes a connection that is idle or slow
// to send its head, and answers 408 to a body that stalls.

/// Read until the server closes; how long that took. Panics if the read
/// timeout passes first — the server kept the connection.
/// The stream's read timeout must already be set: once the server has
/// closed it, macOS refuses `setsockopt` on the socket (EINVAL).
fn time_until_closed(stream: &mut TcpStream, started: Instant) -> Duration {
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return started.elapsed(),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return started.elapsed(),
            Err(e) => panic!(
                "server kept the connection open ({e}) after {:?}",
                started.elapsed()
            ),
        }
    }
}

/// Read one response off a keep-alive stream: status and body.
fn read_response(stream: &mut TcpStream) -> Option<(u16, String)> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => return None,
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            let len = text[..end]
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if text.len() >= end + 4 + len {
                let status = text.split_whitespace().nth(1)?.parse().ok()?;
                return Some((status, text[end + 4..end + 4 + len].to_string()));
            }
        }
    }
}

#[test]
fn an_idle_or_slow_head_connection_is_closed_at_the_header_timeout() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(&rig, port, ", \"header_timeout_ms\": 300", "");
    let with_timeout = |s: TcpStream| {
        s.set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap_or_else(|e| panic!("timeout: {e}"));
        s
    };
    // Idle: connected, sends nothing.
    let mut idle = with_timeout(connect(port));
    let started = Instant::now();
    // Slowloris: a head that never finishes.
    let mut slow = with_timeout(connect(port));
    slow.write_all(format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n").as_bytes())
        .unwrap_or_else(|e| panic!("write: {e}"));
    for (name, stream) in [("idle", &mut idle), ("slow head", &mut slow)] {
        let took = time_until_closed(stream, started);
        assert!(took < Duration::from_secs(3), "{name} held for {took:?}");
    }
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn a_body_that_stalls_is_answered_408_at_the_body_timeout() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(&rig, port, ", \"body_timeout_ms\": 300", "");
    // Time the body deadline, not the server's start-up.
    drop(connect(port));
    let started = Instant::now();
    let (status, _head, _body) = http(
        port,
        &format!(
            "POST /slow HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 10\r\nConnection: close\r\n\r\nab"
        ),
    );
    assert_eq!(status, 408, "a stalled body must be answered, not held");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn connections_past_the_cap_wait_when_every_slot_is_in_the_script() {
    let rig = Rig::new();
    let port = Rig::free_port();
    // Two slots, both held by requests the script is working on: nothing
    // is idle, so nothing can be evicted, and a third client waits.
    let child = route_app(
        &rig,
        port,
        ", \"max_connections\": 2",
        "if (req.path.startsWith('/slow')) { await rt.sleep(600); return {status: 200, body: 'slow ' + req.path}; }",
    );
    let slow = |path: &'static str| {
        std::thread::spawn(move || {
            http(
                port,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                ),
            )
        })
    };
    let first = slow("/slow1");
    std::thread::sleep(Duration::from_millis(100));
    let second = slow("/slow2");
    std::thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    let (status, _head, body) = http(
        port,
        &format!("GET /three HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!((status, body.as_str()), (200, "ok /three"));
    // It got in only after a slow request finished, not by evicting one.
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "{:?}",
        started.elapsed()
    );
    for (handle, path) in [(first, "/slow1"), (second, "/slow2")] {
        let (status, _head, body) = handle.join().unwrap_or_else(|_| panic!("{path}"));
        assert_eq!((status, body), (200, format!("slow {path}")));
    }
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

// ---- Every request gets a real answer ---------------------------------------

#[test]
fn a_malformed_handler_answer_is_a_500_and_the_listener_lives() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let manifest = rig.script(
        "m.manifest.json",
        &format!("{{ \"serve\": {{\"host\": \"127.0.0.1\", \"port\": {port}}} }}\n"),
    );
    let script = rig.script(
        "app.js",
        &"const server = rt.serve({host: '127.0.0.1', port: PORT}, async (req) => {\n\
            if (req.path === '/quit') { rt.setTimeout(() => server.close(), 0); return {status: 200, body: 'bye'}; }\n\
            switch (req.path) {\n\
              case '/none': return undefined;\n\
              case '/bad-header': return {status: 200, headers: {'bad header': 'x'}, body: 'nope'};\n\
              case '/bad-status': return {status: 70000, body: 'nope'};\n\
              case '/number-header': return {headers: {'x-n': 5}, body: 'five'};\n\
              default: return {status: 201, body: 'alive'};\n\
            }\n\
          });\n\
          rt.print('jsrt-test: listening\\n');\n"
            .replace("PORT", &port.to_string()),
    );
    let child = rig.spawn_serve(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    let get = |path: &str| {
        http(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        )
    };
    // No answer object, an invalid header name, an out-of-range status:
    // each is a 500 -- never an empty 200, never a hung client.
    for path in ["/none", "/bad-header", "/bad-status"] {
        let (status, _head, body) = get(path);
        assert_eq!(status, 500, "{path}: body {body:?}");
    }
    // A non-string header value is coerced, not a dropped request.
    let (status, _head, body) = get("/number-header");
    assert_eq!((status, body.as_str()), (200, "five"));
    // And the listener is still serving.
    let (status, _head, body) = get("/alive");
    assert_eq!((status, body.as_str()), (201, "alive"));
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("answered 500"),
        "the operator is told why: {}",
        stderr(&out)
    );
}

// ---- Helpers for tests that stop the server themselves ---------------------

/// A serve app with a `/quit` route that closes the listener, plus any
/// extra `routes` (JS `if (...) return ...;` lines run before the default
/// answer). Tests end it by calling [`quit`] once their assertions are
/// done, never on a timer; the run timeout is only a backstop for a test
/// that fails half-way.
fn route_app(rig: &Rig, port: u16, serve_extra: &str, routes: &str) -> ServeChild {
    let manifest = rig.script(
        "routes.manifest.json",
        &format!("{{ \"serve\": {{\"host\": \"127.0.0.1\", \"port\": {port}{serve_extra}}} }}\n"),
    );
    let script = rig.script(
        "routes.js",
        &format!(
            "const server = rt.serve({{host: '127.0.0.1', port: {port}}}, async (req) => {{\n\
               if (req.path === '/quit') {{ rt.setTimeout(() => server.close(), 0); return {{status: 200, body: 'bye'}}; }}\n\
               {routes}\n\
               return {{status: 200, body: 'ok ' + req.path}};\n\
             }});\n\
             rt.print('jsrt-test: listening\\n');\n"
        ),
    );
    rig.spawn_serve(
        &script,
        &["--manifest", script_arg(&manifest), "--timeout", "60000"],
    )
}

fn script_arg(path: &Path) -> &str {
    path.to_str().unwrap_or_else(|| panic!("utf8"))
}

/// Ask a [`route_app`] to close its listener. The answer may race the
/// process exit, so read errors are not failures here; the exit code the
/// caller checks next is.
fn quit(port: u16) {
    if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = s.write_all(b"GET /quit HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink);
    }
}

#[test]
fn a_get_is_answered_while_sixty_four_bodies_stall() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(&rig, port, "", "");
    drop(connect(port));
    // As many half-sent uploads as the dispatch queue is deep, each
    // promising 100 bytes and sending 2.
    let stalled: Vec<TcpStream> = (0..64)
        .map(|_| {
            let mut s = connect(port);
            s.write_all(b"POST /slow HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\nab")
                .unwrap_or_else(|e| panic!("write: {e}"));
            s
        })
        .collect();
    // Let the listener read all 64 heads first; if the GET overtook them
    // this test would pass without exercising the queue at all.
    std::thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    let (status, _head, body) = http(
        port,
        &format!("GET /fast HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    let took = started.elapsed();
    assert_eq!(
        status, 200,
        "a body-less GET must not wait behind stalled bodies; body {body:?}"
    );
    assert!(took < Duration::from_secs(1), "answered after {took:?}");
    drop(stalled);
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn a_client_that_stops_reading_gives_its_slot_back() {
    let rig = Rig::new();
    let port = Rig::free_port();
    // One connection slot; /big answers 32 MiB, more than loopback socket
    // buffers hold, so a client that never reads stalls the write.
    let child = route_app(
        &rig,
        port,
        ", \"max_connections\": 1, \"write_timeout_ms\": 500",
        "if (req.path === '/big') return {status: 200, body: 'x'.repeat(32 * 1024 * 1024)};",
    );
    drop(connect(port));
    let mut stuck = connect(port);
    stuck
        .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap_or_else(|e| panic!("write: {e}"));
    // `stuck` never reads. The next client must still get an answer: the
    // stalled write gives up after 500 ms and frees the only slot.
    let started = Instant::now();
    let (status, _head, body) = http(
        port,
        &format!("GET /small HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    let took = started.elapsed();
    assert_eq!(
        (status, body.as_str()),
        (200, "ok /small"),
        "after {took:?}"
    );
    assert!(took < Duration::from_secs(4), "answered after {took:?}");
    drop(stuck);
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn the_bad_answer_log_line_hides_queries_and_is_rate_limited() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(
        &rig,
        port,
        "",
        "if (req.path.startsWith('/bad')) return undefined;",
    );
    let pad = "p".repeat(2000);
    for _ in 0..30 {
        let (status, _head, _body) = http(
            port,
            &format!(
                "GET /bad/{pad}?token=SEKRET HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(status, 500);
    }
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let err = stderr(&out);
    assert!(!err.contains("SEKRET"), "the query reached the log: {err}");
    // Only the first path segment is named; tokens live deeper (and the
    // padded segment here is the second one).
    assert!(
        !err.contains("ppppp"),
        "a deeper path segment reached the log"
    );
    assert!(
        err.contains("GET /bad/..."),
        "the first segment is named: {err}"
    );
    let lines: Vec<&str> = err.lines().filter(|l| l.contains("answered 500")).collect();
    assert!(
        (1..=10).contains(&lines.len()),
        "{} answered-500 lines",
        lines.len()
    );
    assert!(
        lines.iter().all(|l| l.len() <= 400),
        "a client-sized path made a long line: {}",
        lines.iter().map(|l| l.len()).max().unwrap_or(0)
    );
    assert!(
        err.contains("20 more"),
        "the suppressed count is reported: {err}"
    );
}

#[test]
fn an_out_of_range_serve_bound_is_a_manifest_error() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let script = rig.script(
        "s.js",
        &format!("const s = rt.serve({{host: '127.0.0.1', port: {port}}}, async () => ({{}}));\ns.close();\n"),
    );
    // 2^61 is past tokio's semaphore limit (it used to abort the host,
    // exit 134); 70000 is past the documented bound. A max_body past 1 GiB
    // could never be reserved from the body budget in one go.
    for (field, value) in [
        ("max_connections", "2305843009213693952"),
        ("max_connections", "70000"),
        ("max_body", "1073741825"),
    ] {
        let manifest = rig.script(
            "m.manifest.json",
            &format!(
                "{{ \"serve\": {{\"host\": \"127.0.0.1\", \"port\": {port}, \"{field}\": {value}}} }}\n"
            ),
        );
        let out = rig.run(&[
            "run",
            script_arg(&script),
            "--manifest",
            script_arg(&manifest),
        ]);
        assert_eq!(
            out.status.code(),
            Some(8),
            "{field} {value}: {:?} {}",
            out.status,
            stderr(&out)
        );
        assert!(stderr(&out).contains(field), "{}", stderr(&out));
    }
}

#[test]
fn header_arrays_repeat_and_non_scalar_values_are_a_500() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(
        &rig,
        port,
        "",
        "if (req.path === '/cookies') return {headers: {'set-cookie': ['a=1; Path=/', 'b=2; Path=/']}, body: 'c'};\n\
         if (req.path === '/obj-header') return {headers: {'x-obj': {a: 1}}, body: 'nope'};\n\
         if (req.path === '/fn-header') return {headers: {'x-fn': () => 1}, body: 'nope'};\n\
         if (req.path === '/nested-array') return {headers: {'x-a': [['x']]}, body: 'nope'};",
    );
    let mut s = connect(port);
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    s.write_all(b"GET /cookies HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap_or_else(|e| panic!("write: {e}"));
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    let cookies: Vec<&str> = raw
        .lines()
        .filter_map(|l| l.strip_prefix("set-cookie: "))
        .collect();
    assert_eq!(cookies, ["a=1; Path=/", "b=2; Path=/"], "response: {raw:?}");
    for path in ["/obj-header", "/fn-header", "/nested-array"] {
        let (status, _head, body) = http(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(status, 500, "{path}: {body:?}");
    }
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

// ---- The body budget never deadlocks and never convoys ---------------------

/// One upload that announces itself with `Expect: 100-continue`, as real
/// clients do for large bodies: the body is sent only after the server
/// says `100 Continue`, so a refusal arrives as a readable answer instead
/// of a reset mid-upload. Returns the final status and the head.
fn upload(port: u16, body: &[u8]) -> (u16, String) {
    let mut s = connect(port);
    s.set_read_timeout(Some(Duration::from_secs(40)))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    s.write_all(
        format!(
            "POST /up HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )
    .unwrap_or_else(|e| panic!("write head: {e}"));
    let read_head = |s: &mut TcpStream| -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match s.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => break,
            }
        }
        String::from_utf8_lossy(&head).into_owned()
    };
    let status_of = |head: &str| -> u16 {
        head.split_whitespace()
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let mut head = read_head(&mut s);
    if status_of(&head) == 100 {
        if s.write_all(body).is_err() {
            return (0, "body write failed".into());
        }
        head = read_head(&mut s);
    }
    (status_of(&head), head)
}

#[test]
fn concurrent_large_uploads_finish_or_are_refused_fast_never_deadlock() {
    let rig = Rig::new();
    let port = Rig::free_port();
    // A long body timeout: a deadlock would show as a 20 s wait and 408s.
    let child = route_app(
        &rig,
        port,
        ", \"body_timeout_ms\": 20000",
        "if (req.path === '/up') return {status: 200, body: 'got ' + req.body.length};",
    );
    // 24 uploads of 7.5 MiB each (180 MiB) against the 64 MiB budget.
    let body = std::sync::Arc::new(vec![b'u'; 7_864_320]);
    let started = Instant::now();
    let workers: Vec<_> = (0..24)
        .map(|_| {
            let body = std::sync::Arc::clone(&body);
            std::thread::spawn(move || upload(port, &body))
        })
        .collect();
    let results: Vec<(u16, String)> = workers
        .into_iter()
        .map(|w| {
            w.join()
                .unwrap_or_else(|_| panic!("upload thread panicked"))
        })
        .collect();
    let took = started.elapsed();
    let ok = results.iter().filter(|r| r.0 == 200).count();
    let busy: Vec<&String> = results
        .iter()
        .filter(|r| r.0 == 503)
        .map(|r| &r.1)
        .collect();
    let statuses: Vec<u16> = results.iter().map(|r| r.0).collect();
    assert_eq!(
        ok + busy.len(),
        24,
        "only 200 or 503, never 408: {statuses:?}"
    );
    assert!(ok >= 1, "some uploads must get through: {statuses:?}");
    assert!(
        busy.iter()
            .all(|h| h.to_ascii_lowercase().contains("retry-after:")),
        "a 503 says when to retry"
    );
    assert!(
        took < Duration::from_secs(10),
        "took {took:?}: {statuses:?}"
    );
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn a_small_post_answers_while_large_bodies_stall() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(&rig, port, "", "");
    // 16 uploads that each declare 8 MiB, send all but the last byte and
    // stop: together they want twice the 64 MiB budget. Each runs on its
    // own thread and keeps its socket open; a writer the server stops
    // reading from just blocks, which is the point.
    let almost = std::sync::Arc::new(vec![b's'; 8 * 1024 * 1024 - 1]);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    for _ in 0..16 {
        let almost = std::sync::Arc::clone(&almost);
        let held_tx = held_tx.clone();
        std::thread::spawn(move || {
            let mut s = connect(port);
            let _ = s.write_all(
                format!(
                    "POST /big HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
                    8 * 1024 * 1024
                )
                .as_bytes(),
            );
            let _ = s.write_all(&almost);
            // Kept open, one byte short, until the test is done.
            let _ = held_tx.send(s);
        });
    }
    // Let the uploads take (or be refused) their budget first.
    std::thread::sleep(Duration::from_secs(2));
    let started = Instant::now();
    let (status, _head, body) = http(
        port,
        &format!(
            "POST /small HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
        ),
    );
    let took = started.elapsed();
    assert_eq!(
        (status, body.as_str()),
        (200, "ok /small"),
        "after {took:?}"
    );
    assert!(took < Duration::from_secs(1), "answered after {took:?}");
    // Close the stalled uploads, then the server.
    for _ in 0..16 {
        if held_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            break;
        }
    }
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn a_chunked_body_past_the_free_share_is_refused_fast_when_the_budget_is_full() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(&rig, port, ", \"body_timeout_ms\": 20000", "");
    // Fill the budget: 8 uploads declaring 8 MiB each and sending a little.
    let fillers: Vec<TcpStream> = (0..8)
        .map(|_| {
            let mut s = connect(port);
            s.write_all(
                format!(
                    "POST /fill HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\nxxxx",
                    8 * 1024 * 1024
                )
                .as_bytes(),
            )
            .unwrap_or_else(|e| panic!("write: {e}"));
            s
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    // A chunked body: 64 KiB is free of the budget, 128 KiB is not.
    let chunk = "c".repeat(64 * 1024);
    let chunked = |chunks: usize| {
        let mut req = format!(
            "POST /chunked HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        );
        for _ in 0..chunks {
            req.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
        }
        req.push_str("0\r\n\r\n");
        req
    };
    let started = Instant::now();
    let (status, _head, _body) = http(port, &chunked(2));
    assert_eq!(
        status, 503,
        "a chunked body over the free share, budget full"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    let (status, _head, body) = http(port, &chunked(1));
    assert_eq!((status, body.as_str()), (200, "ok /chunked"));
    drop(fillers);
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn the_suppressed_line_count_survives_the_worker_dying() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(
        &rig,
        port,
        "",
        "if (req.path === '/bad') return undefined;\n\
         if (req.path === '/die') { rt.setTimeout(() => { throw new Error('worker dies'); }, 0); return {status: 200, body: 'dying'}; }",
    );
    for _ in 0..15 {
        let (status, _head, _body) = http(
            port,
            &format!("GET /bad HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(status, 500);
    }
    // The run ends with an uncaught exception: no window rolls over and
    // the listener never closes, yet the 5 held lines must be reported.
    let _ = http(
        port,
        &format!("GET /die HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    let err = stderr(&out);
    assert_eq!(code(&out), 5, "stderr: {err}");
    assert!(err.contains("worker dies"), "stderr: {err}");
    assert!(
        err.contains("5 more such lines suppressed"),
        "stderr: {err}"
    );
}

#[test]
fn close_ends_the_server_while_a_keep_alive_client_stays_connected() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let mut child = route_app(&rig, port, "", "");
    // One answered request on a keep-alive connection the client keeps.
    let mut kept = connect(port);
    kept.set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    kept.write_all(b"GET /one HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap_or_else(|e| panic!("write: {e}"));
    assert_eq!(read_response(&mut kept).map(|r| r.0), Some(200));
    // server.close() must end the script's serve loop (server.done) even
    // though this connection is still open.
    let started = Instant::now();
    quit(port);
    let exited = loop {
        if child.try_wait_exited() {
            break true;
        }
        if started.elapsed() > Duration::from_secs(5) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(exited, "the server was still running 5 s after close()");
    drop(kept);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

// ---- A reservation must be earned by bytes ---------------------------------

/// Connections that declare `len` bytes and then send nothing.
fn idle_declarers(port: u16, count: usize, len: u64) -> Vec<TcpStream> {
    (0..count)
        .map(|_| {
            let mut s = connect(port);
            s.write_all(
                format!("POST /idle HTTP/1.1\r\nHost: x\r\nContent-Length: {len}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap_or_else(|e| panic!("write: {e}"));
            s
        })
        .collect()
}

#[test]
fn idle_declarations_give_the_budget_back_at_the_first_byte_deadline() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(
        &rig,
        port,
        ", \"body_first_byte_ms\": 500",
        "if (req.path === '/up') return {status: 200, body: 'got ' + req.body.length};",
    );
    // Eight heads declaring 8 MiB each reserve the whole 64 MiB budget
    // and never send a byte.
    let idle = idle_declarers(port, 8, 8 * 1024 * 1024);
    std::thread::sleep(Duration::from_millis(1500));
    // Past the first-byte deadline their reservations are gone.
    let (status, head) = upload(port, &vec![b'u'; 1024 * 1024]);
    assert_eq!(status, 200, "a 1 MiB upload after the deadline: {head:?}");
    drop(idle);
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

/// The process's virtual size in KiB, from `ps`.
fn vsz_kib(pid: u32) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "vsz=", "-p", &pid.to_string()])
        .output()
        .unwrap_or_else(|e| panic!("ps: {e}"));
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("ps vsz: {e}"))
}

#[test]
fn an_idle_large_declaration_is_not_allocated_up_front() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(&rig, port, ", \"max_body\": 1073741824", "");
    drop(connect(port));
    std::thread::sleep(Duration::from_millis(300));
    let before = vsz_kib(child.pid());
    let idle = idle_declarers(port, 1, 1024 * 1024 * 1024);
    std::thread::sleep(Duration::from_millis(500));
    let after = vsz_kib(child.pid());
    let grown_mib = after.saturating_sub(before) / 1024;
    assert!(
        grown_mib < 256,
        "one idle 1 GiB declaration grew the server by {grown_mib} MiB of virtual memory"
    );
    drop(idle);
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

// ---- At the connection cap, idle sockets make room -------------------------

#[test]
fn at_the_cap_the_longest_idle_connection_makes_room() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(&rig, port, ", \"max_connections\": 16", "");
    // Fill every slot: 8 sockets that never send, 8 slow small bodies.
    let mut held: Vec<TcpStream> = (0..8).map(|_| connect(port)).collect();
    for _ in 0..8 {
        let mut s = connect(port);
        s.write_all(b"POST /slow HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\nab")
            .unwrap_or_else(|e| panic!("write: {e}"));
        held.push(s);
    }
    // Let the listener accept all sixteen first.
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let (status, _head, body) = http(
        port,
        &format!("GET /new HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    let took = started.elapsed();
    assert_eq!((status, body.as_str()), (200, "ok /new"), "after {took:?}");
    assert!(took < Duration::from_secs(1), "answered after {took:?}");
    drop(held);
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn eviction_spares_a_request_the_script_is_handling() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(
        &rig,
        port,
        ", \"max_connections\": 2",
        "if (req.path === '/slow') { await rt.sleep(1500); return {status: 200, body: 'slow done'}; }",
    );
    // Slot 1: a request the handler is still working on.
    let busy = std::thread::spawn(move || {
        http(
            port,
            &format!("GET /slow HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        )
    });
    std::thread::sleep(Duration::from_millis(200));
    // Slot 2: an idle socket.
    let mut idle = connect(port);
    idle.set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    std::thread::sleep(Duration::from_millis(200));
    // A newcomer arrives at the cap. The idle socket gives way at once
    // (the server closes it); the busy request does not. The newcomer's
    // answer itself waits for the slow handler, because the script's serve
    // loop handles one request at a time.
    let newcomer = std::thread::spawn(move || {
        http(
            port,
            &format!("GET /new HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        )
    });
    let started = Instant::now();
    let closed = time_until_closed(&mut idle, started);
    assert!(
        closed < Duration::from_secs(1),
        "the idle socket was closed after {closed:?}"
    );
    let (status, _head, body) = busy.join().unwrap_or_else(|_| panic!("busy client"));
    assert_eq!((status, body.as_str()), (200, "slow done"));
    let (status, _head, body) = newcomer.join().unwrap_or_else(|_| panic!("newcomer"));
    assert_eq!((status, body.as_str()), (200, "ok /new"));
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn a_refused_upload_without_expect_still_reads_its_503() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(&rig, port, "", "");
    // Fill the budget for the 5 s first-byte allowance.
    let idle = idle_declarers(port, 8, 8 * 1024 * 1024);
    std::thread::sleep(Duration::from_millis(300));
    // A plain client: head and 1 MiB body in one go, then read. If the
    // server answers and closes without reading, the client's write fails
    // or the 503 is lost to a reset.
    let mut s = connect(port);
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    s.set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    let body = vec![b'p'; 1024 * 1024];
    let mut request = format!(
        "POST /up HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    let wrote = s.write_all(&request);
    let mut answer = Vec::new();
    let read = s.read_to_end(&mut answer);
    let text = String::from_utf8_lossy(&answer).to_ascii_lowercase();
    assert!(wrote.is_ok(), "the upload was cut off: {wrote:?}");
    assert!(read.is_ok() || !answer.is_empty(), "no answer: {read:?}");
    assert!(text.starts_with("http/1.1 503"), "{text:?}");
    assert!(text.contains("retry-after: 1"), "{text:?}");
    drop(idle);
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

// ---- Request-head bounds ---------------------------------------------------

/// Send one raw request on a fresh connection and read the answer, if
/// any. `None` means the server closed (or reset) the connection without
/// one: a head refused while the client was still sending it can lose its
/// 431 to the reset that unread bytes cause on close.
fn raw_status(port: u16, request: &[u8]) -> Option<u16> {
    let mut stream = connect(port);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    let _ = stream.write_all(request);
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => panic!("read: {e} after {} bytes", raw.len()),
        }
        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&raw)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
}

/// A GET whose head carries `extra` headers (besides Host) of `value`.
fn head_with(port: u16, extra: usize, value: &str) -> Vec<u8> {
    let mut head = format!("GET /h HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for i in 0..extra {
        head.push_str(&format!("x-h{i}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    head.into_bytes()
}

#[test]
fn an_oversized_head_or_too_many_headers_is_refused_and_the_listener_keeps_serving() {
    let rig = Rig::new();
    let port = Rig::free_port();
    // Every request the script sees is counted; `/count` reports it.
    let child = route_app(
        &rig,
        port,
        "",
        "if (req.path === '/count') return {status: 200, body: String(globalThis.seen || 0)};\n\
         globalThis.seen = (globalThis.seen || 0) + 1;",
    );
    let count = || {
        let (status, _head, body) = http(
            port,
            &format!("GET /count HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(status, 200, "the listener stopped answering: {body:?}");
        body
    };

    // At the bounds: 64 header lines in all (Host, 62 more, Connection),
    // and a 12 KiB head, are served.
    assert_eq!(raw_status(port, &head_with(port, 62, "v")), Some(200));
    assert_eq!(
        raw_status(port, &head_with(port, 1, &"a".repeat(12 * 1024))),
        Some(200)
    );
    assert_eq!(count(), "2");

    // One header past the cap: 431 before the script sees it.
    assert_eq!(raw_status(port, &head_with(port, 63, "v")), Some(431));
    // Past hyper's own parse limit too.
    assert_eq!(raw_status(port, &head_with(port, 150, "v")), Some(431));
    // A 17 KiB head: refused (431, or closed when the reset beats it).
    let big = raw_status(port, &head_with(port, 1, &"a".repeat(17 * 1024)));
    assert!(matches!(big, Some(431) | None), "a 17 KiB head got {big:?}");
    // The refused requests never reached the script, and it still answers.
    assert_eq!(count(), "2");

    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

// ---- Eviction at the cap picks the stalled head, not honest work ----------

#[test]
fn at_the_cap_a_trickler_cannot_steer_eviction_onto_a_download_being_written() {
    const BIG: usize = 16 * 1024 * 1024;
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(
        &rig,
        port,
        ", \"max_connections\": 2",
        &format!("if (req.path === '/big') return {{status: 200, body: 'd'.repeat({BIG})}};"),
    );
    // Slot 1: an honest download that reads a little, then pauses for
    // 1.5 s, long enough for the server's write to stall on it.
    let download = std::thread::spawn(move || {
        let mut s = connect(port);
        s.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap_or_else(|e| panic!("timeout: {e}"));
        s.write_all(
            format!("GET /big HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let mut got = vec![0u8; 64 * 1024];
        let mut total = s.read(&mut got).unwrap_or(0);
        std::thread::sleep(Duration::from_millis(1500));
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n,
            }
        }
        total
    });
    std::thread::sleep(Duration::from_millis(300));
    // Slot 2: a head that trickles one byte every 200 ms, so its last
    // byte is always fresher than the paused download's.
    let mut trickler = connect(port);
    trickler
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let feeder = {
        let mut t = trickler
            .try_clone()
            .unwrap_or_else(|e| panic!("clone: {e}"));
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            for b in b"GET /slow HTTP/1.1\r\nHost: x\r\nX-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" {
                if stop.load(std::sync::atomic::Ordering::SeqCst) || t.write_all(&[*b]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        })
    };
    std::thread::sleep(Duration::from_millis(600));
    // A newcomer at the cap: the server must close the trickler, not the
    // download it is in the middle of writing.
    let (status, _head, body) = http(
        port,
        &format!("GET /new HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!((status, body.as_str()), (200, "ok /new"));
    let total = download
        .join()
        .unwrap_or_else(|_| panic!("download thread"));
    assert!(
        total > BIG,
        "the download was cut off after {total} of {BIG} body bytes"
    );
    let closed = time_until_closed(&mut trickler, Instant::now());
    assert!(
        closed < Duration::from_secs(2),
        "the trickler stayed {closed:?}"
    );
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = feeder.join();
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

// ---- The body rate rule applies only to bodies that hold budget ------------

/// Send a POST head, then `body` in `pieces` equal parts `gap` apart, and
/// read the answer's status and body.
fn slow_post(port: u16, chunked: bool, body: &[u8], pieces: usize, gap: Duration) -> (u16, String) {
    let mut s = connect(port);
    s.set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    let framing = if chunked {
        "Transfer-Encoding: chunked".to_string()
    } else {
        format!("Content-Length: {}", body.len())
    };
    s.write_all(
        format!(
            "POST /up HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{framing}\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap_or_else(|e| panic!("write head: {e}"));
    for piece in body.chunks(body.len().div_ceil(pieces)) {
        std::thread::sleep(gap);
        let sent = if chunked {
            let mut frame = format!("{:x}\r\n", piece.len()).into_bytes();
            frame.extend_from_slice(piece);
            frame.extend_from_slice(b"\r\n");
            s.write_all(&frame)
        } else {
            s.write_all(piece)
        };
        if sent.is_err() {
            break; // answered early; read what it said
        }
    }
    if chunked {
        let _ = s.write_all(b"0\r\n\r\n");
    }
    read_response(&mut s).unwrap_or((0, String::new()))
}

#[test]
fn a_slow_small_body_gets_the_body_timeout_not_the_rate_deadline() {
    let rig = Rig::new();
    let port = Rig::free_port();
    let child = route_app(
        &rig,
        port,
        ", \"body_first_byte_ms\": 300, \"body_timeout_ms\": 10000",
        "if (req.path === '/up') return {status: 200, body: 'got ' + req.body.length};",
    );
    let gap = Duration::from_millis(250);
    // 8 KiB in eight pieces over 2 s: inside the free 64 KiB, so it holds
    // no budget and only body_timeout (10 s) applies. The rate rule (300 ms
    // plus 8 KiB at 64 KiB/s) would have cut it at about half a second.
    let small = vec![b's'; 8 * 1024];
    assert_eq!(
        slow_post(port, false, &small, 8, gap),
        (200, "got 8192".to_string()),
        "declared length"
    );
    assert_eq!(
        slow_post(port, true, &small, 8, gap),
        (200, "got 8192".to_string()),
        "chunked"
    );
    // A body that reserves budget still has to earn it: 1 MiB declared,
    // sent in pieces 250 ms apart, falls behind 64 KiB/s and gets 408.
    let large = vec![b'l'; 1024 * 1024];
    let (status, _) = slow_post(port, false, &large, 64, gap);
    assert_eq!(status, 408, "a slow 1 MiB body");
    quit(port);
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

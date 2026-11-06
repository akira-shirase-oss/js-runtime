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

    /// A free loopback port: bind :0, read the port, drop. Small race —
    /// fine on loopback for tests, and retry covers the rare collision.
    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|e| panic!("bind :0: {e}"))
            .local_addr()
            .unwrap_or_else(|e| panic!("local_addr: {e}"))
            .port()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(JSRT)
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .unwrap_or_else(|e| panic!("spawn jsrt: {e}"))
    }

    /// Spawn a serve script in the background; caller polls the port.
    fn spawn_serve(&self, script: &Path, extra: &[&str]) -> Child {
        let mut args = vec!["run"];
        args.push(script.to_str().unwrap_or_else(|| panic!("utf8")));
        args.extend_from_slice(extra);
        Command::new(JSRT)
            .args(&args)
            .current_dir(self.dir.path())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn jsrt: {e}"))
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
           if (req.path === '/boom') throw new Error('handler died');\n\
           return {status: 201, headers: {'x-jsrt': 'y'}, body: req.method + ' ' + req.path};\n\
         });\n\
         rt.setTimeout(() => server.close(), 3000);\n"
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
           seen++;\n\
           return {status: 200, body: 'seen:' + seen + ' len:' + req.body.length};\n\
         });\n\
         rt.setTimeout(() => server.close(), 3000);\n"
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
        "const s = rt.serve({host: '127.0.0.1', port: PORT}, async (r) => ({status: 200, body: 'flag'}));\n\
         rt.setTimeout(() => s.close(), 3000);\n"
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

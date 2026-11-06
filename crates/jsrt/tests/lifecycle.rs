//! The exit-class contract, proven end to end against the real binary.
//!
//! Every way a run can end gets a test that produces it honestly — no stubs:
//! a real `while(1){}` for termination, a real uncaught `CapabilityError` for
//! capability, a real unreadable manifest for host. If a class can't be
//! produced, the taxonomy is lying and the test should fail.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

    /// Write `source` at an absolute path inside the rig (e.g. `app/main.js`).
    fn script_at(&self, path: &Path, source: &str) -> PathBuf {
        fs::write(path, source).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        path.to_path_buf()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(JSRT)
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .unwrap_or_else(|e| panic!("spawn jsrt: {e}"))
    }

    fn run_script(&self, script: &Path, extra: &[&str]) -> Output {
        let mut args = vec!["run"];
        args.push(
            script
                .to_str()
                .unwrap_or_else(|| panic!("script path utf8")),
        );
        args.extend_from_slice(extra);
        self.run(&args)
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

#[test]
fn a_clean_run_exits_zero() {
    let rig = Rig::new();
    let script = rig.script("ok.js", "rt.print('hi');\nawait rt.sleep(1);\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
}

#[test]
fn a_missing_file_is_a_load_error() {
    let rig = Rig::new();
    let out = rig.run(&["run", "nope.js"]);
    assert_eq!(code(&out), 3);
    assert!(stderr(&out).contains("load"), "stderr: {}", stderr(&out));
}

#[test]
fn a_syntax_error_is_not_an_exception() {
    let rig = Rig::new();
    let script = rig.script("bad.js", "const x = ;\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 4);
    assert!(stderr(&out).contains("syntax"), "stderr: {}", stderr(&out));
}

#[test]
fn an_uncaught_throw_is_an_exception() {
    let rig = Rig::new();
    let script = rig.script("throw.js", "throw new Error('boom');\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 5);
    assert!(
        stderr(&out).contains("exception"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_rejected_promise_is_an_exception_too() {
    let rig = Rig::new();
    let script = rig.script(
        "reject.js",
        "await Promise.reject(new Error('async boom'));\n",
    );
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 5, "stderr: {}", stderr(&out));
}

#[test]
fn a_cpu_bound_loop_dies_terminated() {
    let rig = Rig::new();
    let script = rig.script("spin.js", "while (true) {}\n");
    let out = rig.run_script(&script, &["--timeout", "400"]);
    assert_eq!(code(&out), 6, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("terminated"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_stalled_promise_dies_terminated() {
    let rig = Rig::new();
    let script = rig.script("stall.js", "await new Promise(() => {});\n");
    let out = rig.run_script(&script, &["--timeout", "400"]);
    assert_eq!(code(&out), 6, "stderr: {}", stderr(&out));
}

#[test]
fn an_uncaught_capability_refusal_exits_seven() {
    let rig = Rig::new();
    // deny-all manifest: every gated op refuses.
    let script = rig.script("read.js", "await rt.readText('/etc/passwd');\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("capability"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_caught_capability_refusal_is_fine() {
    let rig = Rig::new();
    let script = rig.script(
        "catch.js",
        "try { await rt.readText('/etc/passwd'); } catch (e) { rt.print(e.name + ': ' + e.message); }\n",
    );
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("CapabilityError"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_broken_manifest_is_a_host_error() {
    let rig = Rig::new();
    let script = rig.script("ok.js", "rt.print('hi');\n");
    let out = rig.run_script(&script, &["--manifest", "missing.manifest.json"]);
    assert_eq!(code(&out), 8, "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("host"), "stderr: {}", stderr(&out));
}

#[test]
fn clap_usage_errors_exit_two() {
    let rig = Rig::new();
    let out = rig.run(&["run", "--timeout", "notanumber", "x.js"]);
    assert_eq!(code(&out), 2);
}

#[test]
fn a_manifest_read_grant_works() {
    let rig = Rig::new();
    let data = rig.dir.path().join("data");
    fs::create_dir(&data).unwrap_or_else(|e| panic!("mkdir: {e}"));
    fs::write(data.join("note.txt"), "inside the fence").unwrap_or_else(|e| panic!("write: {e}"));
    let manifest = rig.script("m.manifest.json", "{ \"read\": [\"data\"] }\n");
    let script = rig.script("read.js", "rt.print(await rt.readText('data/note.txt'));\n");
    let out = rig.run_script(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("inside the fence"));
}

#[test]
fn a_read_outside_the_grant_still_refuses() {
    let rig = Rig::new();
    let data = rig.dir.path().join("data");
    fs::create_dir(&data).unwrap_or_else(|e| panic!("mkdir: {e}"));
    fs::write(rig.dir.path().join("secret.txt"), "outside")
        .unwrap_or_else(|e| panic!("write: {e}"));
    let manifest = rig.script("m.manifest.json", "{ \"read\": [\"data\"] }\n");
    // `..` escape attempt must also refuse — canonicalization eats it.
    let script = rig.script("read.js", "await rt.readText('data/../secret.txt');\n");
    let out = rig.run_script(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
}

#[test]
fn a_host_side_failure_exits_eight() {
    let rig = Rig::new();
    let data = rig.dir.path().join("data");
    fs::create_dir(&data).unwrap_or_else(|e| panic!("mkdir: {e}"));
    let manifest = rig.script("m.manifest.json", "{ \"read\": [\"data\"] }\n");
    // `data` is granted and canonicalizes, but reading a directory fails at
    // the host — a HostError (8), not a refusal (7) or a script error (5).
    let script = rig.script("read.js", "await rt.readText('data');\n");
    let out = rig.run_script(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    assert_eq!(code(&out), 8, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("HostError"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_multi_file_import_runs() {
    let rig = Rig::new();
    rig.script("lib.js", "export function greet(n) { return 'hi ' + n; }\n");
    let script = rig.script(
        "main.js",
        "import { greet } from './lib.js';\nrt.print(greet('mod'));\n",
    );
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("hi mod"));
}

#[test]
fn a_missing_import_is_a_load_error_naming_the_specifier() {
    let rig = Rig::new();
    let script = rig.script("main.js", "import './missing.js';\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 3, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("missing.js"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_bad_specifier_is_a_load_error_naming_it() {
    let rig = Rig::new();
    let script = rig.script("main.js", "import 'ht!tp:bad';\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 3, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("ht!tp:bad"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_syntax_error_in_a_dependency_is_not_a_load_error() {
    let rig = Rig::new();
    rig.script("dep.js", "const = 3;\n");
    let script = rig.script("main.js", "import './dep.js';\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 4, "stderr: {}", stderr(&out));
}

#[test]
fn a_circular_import_pair_evaluates() {
    let rig = Rig::new();
    // ESM circularity is legal; bindings resolve live. Pin the real behavior —
    // both entry directions complete and both see each other's exports.
    rig.script(
        "a.js",
        "import { b } from './b.js';\nexport function a() { return 'a'; }\nrt.print('a sees b=' + b());\n",
    );
    rig.script(
        "b.js",
        "import { a } from './a.js';\nexport function b() { return 'b'; }\nrt.print('b sees a=' + a());\n",
    );
    for entry in ["a.js", "b.js"] {
        let path = rig.dir.path().join(entry);
        let out = rig.run_script(&path, &[]);
        assert_eq!(code(&out), 0, "{entry}: stderr: {}", stderr(&out));
    }
}

#[test]
fn a_pending_timeout_holds_the_loop_then_fires() {
    let rig = Rig::new();
    let script = rig.script(
        "t.js",
        "rt.setTimeout(() => rt.print('fired\\n'), 30);\nrt.print('end\\n');\n",
    );
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // 'end' prints first, 'fired' second — the loop stayed alive for it.
    let end = stdout
        .find("end")
        .unwrap_or_else(|| panic!("stdout: {stdout}"));
    let fired = stdout
        .find("fired")
        .unwrap_or_else(|| panic!("stdout: {stdout}"));
    assert!(end < fired, "stdout: {stdout}");
}

#[test]
fn a_cleared_timeout_never_fires() {
    let rig = Rig::new();
    let script = rig.script(
        "t.js",
        "const id = rt.setTimeout(() => rt.print('never\\n'), 30);\nrt.clearTimeout(id);\n",
    );
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("never"));
}

#[test]
fn an_interval_repeats_until_cleared() {
    let rig = Rig::new();
    let script = rig.script(
        "t.js",
        "let n = 0;\nconst id = rt.setInterval(() => { if (++n === 3) { rt.clearInterval(id); rt.print('done ' + n + '\\n'); } }, 5);\n",
    );
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("done 3"));
}

#[test]
fn an_unrefed_timer_does_not_hold_the_loop() {
    let rig = Rig::new();
    // 60s timer, unref'd: exits now. If unref were broken the run would park
    // and the 400ms watchdog (with 2s grace) would hand back exit 6.
    let script = rig.script(
        "t.js",
        "const id = rt.setTimeout(() => rt.print('never\\n'), 60000);\nrt.unrefTimer(id);\nrt.print('out\\n');\n",
    );
    let out = rig.run_script(&script, &["--timeout", "400"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("out"));
}

#[test]
fn a_pending_refed_timer_dies_with_the_watchdog() {
    let rig = Rig::new();
    let script = rig.script(
        "t.js",
        "rt.setTimeout(() => rt.print('never\\n'), 60000);\n",
    );
    let out = rig.run_script(&script, &["--timeout", "400"]);
    assert_eq!(code(&out), 6, "stderr: {}", stderr(&out));
}

#[test]
fn a_throwing_timer_callback_is_an_exception() {
    let rig = Rig::new();
    let script = rig.script(
        "t.js",
        "rt.setTimeout(() => { throw new Error('cb boom'); }, 5);\n",
    );
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 5, "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("cb boom"), "stderr: {}", stderr(&out));
}

#[test]
fn there_are_no_bare_timer_globals() {
    let rig = Rig::new();
    // Everything a script can do lives under rt.* — no ambient setTimeout.
    let script = rig.script("t.js", "setTimeout(() => {}, 1);\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 5, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("setTimeout is not defined"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn cli_flags_extend_never_weaken() {
    let rig = Rig::new();
    let data = rig.dir.path().join("data");
    fs::create_dir(&data).unwrap_or_else(|e| panic!("mkdir: {e}"));
    fs::write(data.join("note.txt"), "via flag").unwrap_or_else(|e| panic!("write: {e}"));
    // Manifest grants nothing; --read adds the dir.
    let manifest = rig.script("m.manifest.json", "{}\n");
    let script = rig.script("read.js", "rt.print(await rt.readText('data/note.txt'));\n");
    let out = rig.run_script(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
            "--read",
            data.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

// ---- M2.2: bounded fetch, against a real TCP stub ---------------------------

use std::io::{Read as _, Write as _};
use std::net::TcpListener;

/// A one-connection-at-a-time HTTP stub. `serve` decides the response; the
/// listener lives until the test ends (daemon thread).
struct Stub {
    port: u16,
}

impl Stub {
    fn start(serve: impl Fn(&mut std::net::TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("bind: {e}"));
        let port = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("addr: {e}"))
            .port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(mut s) => serve(&mut s),
                    Err(_) => return,
                }
            }
        });
        Self { port }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

/// Read the request headers, then write a fixed `status`+`body` response.
fn respond(status: &str, body: &[u8], delay_ms: u64) -> impl Fn(&mut std::net::TcpStream) {
    let status = status.to_string();
    let body = body.to_vec();
    move |s| {
        let mut buf = [0u8; 2048];
        let _ = s.read(&mut buf);
        if delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = s.write_all(head.as_bytes());
        let _ = s.write_all(&body);
    }
}

fn fetch_script(rig: &Rig, url: &str) -> PathBuf {
    rig.script(
        "f.js",
        &format!(
            "const r = await rt.fetch('{url}');\nrt.print('status=' + r.status + ' body=' + r.body + '\\n');\n"
        ),
    )
}

#[test]
fn a_declared_fetch_returns_the_response() {
    let rig = Rig::new();
    let stub = Stub::start(respond("200 OK", b"hello fetch", 0));
    let url = stub.url("/ok");
    let script = fetch_script(&rig, &url);
    let out = rig.run_script(&script, &["--net", &format!("127.0.0.1:{}", stub.port)]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("status=200 body=hello fetch"));
}

#[test]
fn an_undeclared_fetch_destination_exits_seven() {
    let rig = Rig::new();
    let stub = Stub::start(respond("200 OK", b"x", 0));
    let url = stub.url("/ok");
    let script = fetch_script(&rig, &url);
    // Same port, but nothing granted — deny by default.
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
}

#[test]
fn a_slow_endpoint_dies_at_the_deadline() {
    let rig = Rig::new();
    let stub = Stub::start(respond("200 OK", b"too slow", 3000));
    let url = stub.url("/slow");
    let script = fetch_script(&rig, &url);
    let out = rig.run_script(
        &script,
        &[
            "--net",
            &format!("127.0.0.1:{}", stub.port),
            "--fetch-timeout",
            "300",
            "--timeout",
            "15000",
        ],
    );
    assert_eq!(code(&out), 8, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("deadline"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn an_authority_smuggle_is_refused_not_connecting_elsewhere() {
    let rig = Rig::new();
    let stub = Stub::start(respond("200 OK", b"x", 0));
    // Grant the stub's host:port, then fetch a URL whose *string* shows a
    // userinfo-looking `granted` authority after a backslash — but whose
    // real destination (per WHATWG parsing, and reqwest) is 127.0.0.1:80.
    // The check must follow the connection, not the string.
    // Rust \\\\ -> JS source \\ -> JS string \: the URL must carry a real
    // backslash, or JS quietly eats it and the test proves nothing.
    let url = format!("http://127.0.0.1\\\\@127.0.0.1:{}/x", stub.port);
    let script = fetch_script(&rig, &url);
    let out = rig.run_script(&script, &["--net", &format!("127.0.0.1:{}", stub.port)]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("127.0.0.1:80"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_fetch_to_a_file_url_is_refused() {
    let rig = Rig::new();
    // file:// never reaches the net check — the scheme gate fails closed.
    let script = rig.script("f.js", "await rt.fetch('file:///etc/passwd');\n");
    let out = rig.run_script(&script, &["--net", "127.0.0.1"]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
}

#[test]
fn a_five_hundred_is_a_result_not_a_crash() {
    let rig = Rig::new();
    let stub = Stub::start(respond("500 Internal Server Error", b"oops", 0));
    let url = stub.url("/err");
    // Fetch semantics: transport succeeded — the status is data for the
    // script, not an exception for the host.
    let script = rig.script(
        "f.js",
        &format!("const r = await rt.fetch('{url}');\nrt.print('status=' + r.status + '\\n');\n"),
    );
    let out = rig.run_script(&script, &["--net", &format!("127.0.0.1:{}", stub.port)]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("status=500"));
}

#[test]
fn an_oversized_body_is_cut_off() {
    let rig = Rig::new();
    let stub = Stub::start(respond("200 OK", &vec![b'x'; 5 * 1024 * 1024], 0));
    let url = stub.url("/big");
    let script = fetch_script(&rig, &url);
    let out = rig.run_script(&script, &["--net", &format!("127.0.0.1:{}", stub.port)]);
    assert_eq!(code(&out), 8, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("exceeded"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_script_can_see_its_own_pending_timers() {
    let rig = Rig::new();
    let script = rig.script(
        "t.js",
        "const id = rt.setTimeout(() => {}, 60000);\n\
         rt.unrefTimer(id);\n\
         rt.print('pending=' + rt.pendingTimers() + '\\n');\n\
         rt.clearTimeout(id);\n\
         rt.print('after=' + rt.pendingTimers() + '\\n');\n",
    );
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("pending=1"), "stdout: {stdout}");
    assert!(stdout.contains("after=0"), "stdout: {stdout}");
}

// ---- M3.1: the loader is inside the fence ------------------------------------
//
// Before this step, `FsModuleLoader` resolved any import to any file on disk —
// `may_read` gated ops but the module graph walked straight past it. Now the
// rule is: the entry's own directory is program-space (implicit, loader-only),
// anything beyond it needs a declared read path, and a canonicalized escape is
// a CapabilityError (7) — while a genuinely missing dep stays a Load error (3).

/// Entry lives in `app/`; its siblings are program-space, anything above is not.
fn app_rig() -> (Rig, PathBuf) {
    let rig = Rig::new();
    let app = rig.dir.path().join("app");
    fs::create_dir(&app).unwrap_or_else(|e| panic!("mkdir app: {e}"));
    (rig, app)
}

#[test]
fn an_import_outside_the_entry_dir_is_a_capability_refusal() {
    let (rig, app) = app_rig();
    fs::write(
        rig.dir.path().join("secret.js"),
        "export const s = 'leak';\n",
    )
    .unwrap_or_else(|e| panic!("write: {e}"));
    let script = rig.script_at(&app.join("main.js"), "import '../secret.js';\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("secret.js"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_missing_import_still_classifies_as_load_not_capability() {
    let (rig, app) = app_rig();
    // Missing: canonicalize fails → the gate passes it to the loader, which
    // reports a real Load error (3) — we do not turn absence into a refusal.
    let script = rig.script_at(&app.join("main.js"), "import '../nope.js';\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 3, "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("nope.js"), "stderr: {}", stderr(&out));
}

#[test]
fn a_manifest_read_grant_opens_the_path_to_imports() {
    let (rig, app) = app_rig();
    let shared = rig.dir.path().join("shared");
    fs::create_dir(&shared).unwrap_or_else(|e| panic!("mkdir shared: {e}"));
    fs::write(shared.join("util.js"), "export const u = 'granted';\n")
        .unwrap_or_else(|e| panic!("write: {e}"));
    let manifest = rig.script("m.manifest.json", "{ \"read\": [\"shared\"] }\n");
    let script = rig.script_at(
        &app.join("main.js"),
        "import { u } from '../shared/util.js';\nrt.print('u=' + u);\n",
    );
    let out = rig.run_script(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("u=granted"));
}

#[test]
fn a_deep_dependency_cannot_launder_an_outside_import() {
    let (rig, app) = app_rig();
    fs::write(rig.dir.path().join("vault.js"), "export const v = 'no';\n")
        .unwrap_or_else(|e| panic!("write: {e}"));
    // lib.js is inside the entry dir and loads fine; its own import is the
    // one that must be refused. The fence applies at every edge, not just
    // the first.
    rig.script_at(&app.join("lib.js"), "export { v } from '../vault.js';\n");
    let script = rig.script_at(&app.join("main.js"), "import './lib.js';\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("vault.js"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_dynamic_import_outside_the_fence_is_also_refused() {
    let (rig, app) = app_rig();
    fs::write(rig.dir.path().join("dyn.js"), "export const d = 'no';\n")
        .unwrap_or_else(|e| panic!("write: {e}"));
    let script = rig.script_at(
        &app.join("main.js"),
        "try { await import('../dyn.js'); rt.print('loaded\\n'); } catch (e) { rt.print(e.name + '|' + e.message + '\\n'); }\n",
    );
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    // Honest quirk: V8 rejects a dynamic import through a path that flattens
    // the class — the script sees TypeError, not CapabilityError. The refusal
    // itself is real (nothing loads) and the message still says why.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("TypeError"), "stdout: {stdout}");
    assert!(
        stdout.contains("outside every declared read path"),
        "stdout: {stdout}"
    );
}

#[test]
fn an_uncaught_dynamic_import_refusal_still_exits_seven() {
    let (rig, app) = app_rig();
    fs::write(rig.dir.path().join("dyn.js"), "export const d = 'no';\n")
        .unwrap_or_else(|e| panic!("write: {e}"));
    // Uncaught: the rejection reaches the host flattened to TypeError, but
    // the classifier recognizes the loader's own refusal prefix — a refusal
    // is a refusal, whichever door it left through.
    let script = rig.script_at(&app.join("main.js"), "await import('../dyn.js');\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("capability"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
#[cfg(unix)]
fn a_symlink_inside_the_fence_cannot_point_out() {
    let (rig, app) = app_rig();
    fs::write(
        rig.dir.path().join("target.js"),
        "export const t = 'esc';\n",
    )
    .unwrap_or_else(|e| panic!("write: {e}"));
    std::os::unix::fs::symlink(rig.dir.path().join("target.js"), app.join("link.js"))
        .unwrap_or_else(|e| panic!("symlink: {e}"));
    // The specifier looks inside the fence; canonicalization reveals the
    // real target outside it. The checked path is the path that loads.
    let script = rig.script_at(&app.join("main.js"), "import './link.js';\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("target.js"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn the_entry_itself_is_never_capability_gated() {
    let (rig, app) = app_rig();
    // Manifest grants only `data/`; the entry at app/main.js is the
    // operator's choice and must still load, evaluate, and use its own dir.
    let data = rig.dir.path().join("data");
    fs::create_dir(&data).unwrap_or_else(|e| panic!("mkdir data: {e}"));
    fs::write(data.join("note.txt"), "via manifest").unwrap_or_else(|e| panic!("write: {e}"));
    let manifest = rig.script("m.manifest.json", "{ \"read\": [\"data\"] }\n");
    rig.script_at(&app.join("lib.js"), "export const l = 'local';\n");
    let script = rig.script_at(
        &app.join("main.js"),
        "import { l } from './lib.js';\nrt.print(l + '|' + await rt.readText('data/note.txt') + '\\n');\n",
    );
    let out = rig.run_script(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("local|via manifest"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_manifest_granted_dir_is_also_importable() {
    let (rig, app) = app_rig();
    // Both an op-read and a module-import land in the same granted dir —
    // one manifest covers the whole surface, not two separate fences.
    let lib = rig.dir.path().join("lib");
    fs::create_dir(&lib).unwrap_or_else(|e| panic!("mkdir lib: {e}"));
    fs::write(lib.join("mod.js"), "export const m = 'mod';\n")
        .unwrap_or_else(|e| panic!("write: {e}"));
    fs::write(lib.join("note.txt"), "note").unwrap_or_else(|e| panic!("write: {e}"));
    let manifest = rig.script("m.manifest.json", "{ \"read\": [\"lib\"] }\n");
    let script = rig.script_at(
        &app.join("main.js"),
        "import { m } from '../lib/mod.js';\nrt.print(m + '|' + await rt.readText('lib/note.txt') + '\\n');\n",
    );
    let out = rig.run_script(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("mod|note"));
}

// ---- rt.kv — the embedded store -------------------------------------------
//
// The honest surface: no grant means CapabilityError, puts persist on disk
// (a second run reads what the first wrote), the quota is the file's own
// size, and list is an exact-prefix scan.

#[test]
fn kv_without_a_grant_refuses() {
    let rig = Rig::new();
    let script = rig.script("s.js", "rt.kv.get('k');\n");
    let out = rig.run_script(&script, &[]);
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("kv denied"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn kv_put_get_delete_roundtrip() {
    let rig = Rig::new();
    fs::create_dir_all(rig.dir.path().join("data")).unwrap_or_else(|e| panic!("mkdir: {e}"));
    let manifest = rig.script(
        "m.manifest.json",
        "{ \"kv\": { \"path\": \"data/app.kv\" } }\n",
    );
    let script = rig.script(
        "s.js",
        "rt.kv.put('a', 'one');\n\
         rt.print(rt.kv.get('a') + '|' + rt.kv.delete('a') + '|' + rt.kv.get('a') + '\\n');\n",
    );
    let out = rig.run_script(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "one|true|null\n");
}

#[test]
fn kv_persists_across_runs() {
    let rig = Rig::new();
    fs::create_dir_all(rig.dir.path().join("data")).unwrap_or_else(|e| panic!("mkdir: {e}"));
    let manifest = rig.script(
        "m.manifest.json",
        "{ \"kv\": { \"path\": \"data/app.kv\" } }\n",
    );
    let mpath = manifest
        .to_str()
        .unwrap_or_else(|| panic!("utf8"))
        .to_string();
    let put = rig.script("put.js", "rt.kv.put('kept', 'across');\n");
    let out = rig.run_script(&put, &["--manifest", &mpath]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let get = rig.script("get.js", "rt.print('read:' + rt.kv.get('kept') + '\\n');\n");
    let out = rig.run_script(&get, &["--manifest", &mpath]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "read:across\n");
}

#[test]
fn kv_quota_refuses_the_write() {
    let rig = Rig::new();
    fs::create_dir_all(rig.dir.path().join("data")).unwrap_or_else(|e| panic!("mkdir: {e}"));
    // One byte of quota: even the db's own pages blow past it.
    let manifest = rig.script(
        "m.manifest.json",
        "{ \"kv\": { \"path\": \"data/app.kv\", \"max_bytes\": 1 } }\n",
    );
    let script = rig.script("s.js", "rt.kv.put('a', 'x');\n");
    let out = rig.run_script(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("kv quota exceeded"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn kv_list_returns_entries_under_an_exact_prefix() {
    let rig = Rig::new();
    fs::create_dir_all(rig.dir.path().join("data")).unwrap_or_else(|e| panic!("mkdir: {e}"));
    let manifest = rig.script(
        "m.manifest.json",
        "{ \"kv\": { \"path\": \"data/app.kv\" } }\n",
    );
    let script = rig.script(
        "s.js",
        "rt.kv.put('user/2', 'b');\n\
         rt.kv.put('user/1', 'a');\n\
         rt.kv.put('other', 'o');\n\
         rt.print(rt.kv.list('user/').map(e => e.key + '=' + e.value).join(',') + '\\n');\n",
    );
    let out = rig.run_script(
        &script,
        &[
            "--manifest",
            manifest.to_str().unwrap_or_else(|| panic!("utf8")),
        ],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "user/1=a,user/2=b\n");
}

#[test]
fn kv_flag_grants_a_store() {
    let rig = Rig::new();
    let db = rig.dir.path().join("flag.kv");
    let script = rig.script(
        "s.js",
        "rt.kv.put('x', 'y'); rt.print(rt.kv.get('x') + '\\n');\n",
    );
    let out = rig.run_script(
        &script,
        &["--kv", db.to_str().unwrap_or_else(|| panic!("utf8"))],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "y\n");
}

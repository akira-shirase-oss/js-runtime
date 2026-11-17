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

// ---- Refusals a script can read name what it asked for, not host paths ----

#[test]
fn caught_refusals_do_not_reveal_canonical_paths_or_symlink_targets() {
    let rig = Rig::new();
    // Outside the program and every grant: a directory whose name the
    // script never sees, except where it asks for a file in it by name.
    let outside = rig.path("outside-HIDDENTARGET");
    // The program, with its grants beside it: links inside the read grant
    // that point out, and a file over the 4 MiB read cap.
    let app = rig.path("app");
    let (data, media) = (app.join("data"), app.join("media"));
    for d in [&outside, &app, &data, &media] {
        fs::create_dir(d).unwrap_or_else(|e| panic!("mkdir {}: {e}", d.display()));
    }
    fs::write(outside.join("secret.txt"), "secret").unwrap_or_else(|e| panic!("write: {e}"));
    fs::write(outside.join("secret.js"), "export default 1;\n")
        .unwrap_or_else(|e| panic!("write: {e}"));
    for (link, target) in [("link.txt", "secret.txt"), ("link.js", "secret.js")] {
        std::os::unix::fs::symlink(outside.join(target), data.join(link))
            .unwrap_or_else(|e| panic!("symlink: {e}"));
    }
    fs::write(data.join("big.txt"), vec![b'b'; 4 * 1024 * 1024 + 1])
        .unwrap_or_else(|e| panic!("write: {e}"));
    let manifest = rig.script(
        "app/m.manifest.json",
        "{ \"read\": [\"./data\"],\n\
           \"kv\": { \"path\": \"data/app.kv\", \"max_bytes\": 1024 },\n\
           \"media\": { \"backend\": \"local\", \"root\": \"media/\", \"max_bytes\": 1048576 } }\n",
    );
    // readText and media paths are relative to the working directory (the
    // rig); import specifiers to the module.
    let script = rig.script(
        "app/probe.js",
        "const probes = [\n\
           ['readText via a link', () => rt.readText('app/data/link.txt')],\n\
           ['readText outside', () => rt.readText('outside-HIDDENTARGET/secret.txt')],\n\
           ['readText over the cap', () => rt.readText('app/data/big.txt')],\n\
           ['media via a link', () => rt.media.put('app/data/link.txt')],\n\
           ['import via a link', () => import('./data/link.js')],\n\
           ['text import over the cap', () => import('./data/big.txt', { with: { type: 'text' } })],\n\
           ['kv over quota', () => rt.kv.put('k', 'v'.repeat(4096))],\n\
         ];\n\
         for (const [name, probe] of probes) {\n\
           try { await probe(); rt.print(name + ' => allowed\\n'); }\n\
           catch (e) { rt.print(name + ' => ' + e.name + ': ' + e.message + '\\n'); }\n\
         }\n",
    );
    let out = rig.run_script(&script, &["--manifest", arg(&manifest)]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let seen = stdout(&out);
    let root = fs::canonicalize(rig.dir.path()).unwrap_or_else(|e| panic!("canonicalize: {e}"));
    let root = root.to_string_lossy();
    for line in seen.lines() {
        assert!(!line.contains("allowed"), "{line}");
        assert!(
            !line.contains(root.as_ref()),
            "a refusal names the host path {root}: {line}"
        );
        if !line.starts_with("readText outside") {
            assert!(
                !line.contains("HIDDENTARGET"),
                "a refusal names a link's target: {line}"
            );
        }
    }
    assert_eq!(seen.lines().count(), 7, "{seen}");
}

#[test]
fn one_call_that_allocates_far_past_the_heap_limit_still_ends_classified() {
    let rig = Rig::new();
    // A single `fill` over 50 million slots: about 700 MB of heap in one
    // builtin call, which returns only once it is done, so the termination
    // the limit asks for cannot land before then.
    let script = rig.script(
        "huge.js",
        "const a = new Array(5e7).fill(1.5);\nrt.print('survived ' + a.length + '\\n');\n",
    );
    let out = rig.run_script(&script, &["--max-heap-mb", "64", "--timeout", "60000"]);
    assert_eq!(
        out.status.code(),
        Some(6),
        "status {:?}, stderr: {}",
        out.status,
        stderr(&out).lines().take(5).collect::<Vec<_>>().join("\n")
    );
    assert!(
        stderr(&out).contains("heap limit"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(!stdout(&out).contains("survived"), "{}", stdout(&out));
}

#[test]
fn a_heap_limit_below_the_floor_is_refused_and_the_floor_works() {
    let rig = Rig::new();
    let hello = rig.script("hello.js", "rt.print('hello\\n');\n");
    // Below the floor V8 cannot even start a script; that is a usage
    // error, not a crash.
    for mb in ["1", "4", "15"] {
        let out = rig.run_script(&hello, &["--max-heap-mb", mb]);
        assert_eq!(
            out.status.code(),
            Some(2),
            "--max-heap-mb {mb}: status {:?}, stderr: {}",
            out.status,
            stderr(&out).lines().take(3).collect::<Vec<_>>().join("\n")
        );
        assert!(
            stderr(&out).contains("at least 16"),
            "--max-heap-mb {mb}: {}",
            stderr(&out)
        );
    }
    // At the floor a script runs, and a heap bomb still ends as exit 6.
    let out = rig.run_script(&hello, &["--max-heap-mb", "16"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "hello\n");
    let bomb = rig.script(
        "bomb.js",
        "const hoard = [];\n\
         while (true) { hoard.push(new Array(100000).fill({ x: hoard.length })); }\n",
    );
    let out = rig.run_script(&bomb, &["--max-heap-mb", "16", "--timeout", "60000"]);
    assert_eq!(
        out.status.code(),
        Some(6),
        "status {:?}, stderr: {}",
        out.status,
        stderr(&out).lines().take(5).collect::<Vec<_>>().join("\n")
    );
}

// ---- A script without a grant cannot tell which host paths exist ----------

#[test]
fn a_script_without_a_read_grant_cannot_tell_which_host_paths_exist() {
    let rig = Rig::new();
    let outside = rig.path("outside");
    let app = rig.path("app");
    for d in [&outside, &app, &app.join("media")] {
        fs::create_dir(d).unwrap_or_else(|e| panic!("mkdir {}: {e}", d.display()));
    }
    fs::write(outside.join("present.txt"), "here").unwrap_or_else(|e| panic!("write: {e}"));
    fs::write(outside.join("present.js"), "export default 1;\n")
        .unwrap_or_else(|e| panic!("write: {e}"));
    // A media store but no read grant: media.put has to refuse its source.
    let manifest = rig.script(
        "app/m.manifest.json",
        "{ \"media\": { \"backend\": \"local\", \"root\": \"media/\" } }\n",
    );
    let outside = outside.to_string_lossy().into_owned();
    // Pairs that differ only in whether the path exists: a file this test
    // made and its missing sibling, a relative spelling of both, and a
    // system file next to a name that is not there.
    let pairs = [
        (
            format!("{outside}/present.txt"),
            format!("{outside}/absent.txt"),
        ),
        (
            format!("{outside}/present.js"),
            format!("{outside}/absent.js"),
        ),
        (
            "../outside/present.js".to_string(),
            "../outside/absent.js".to_string(),
        ),
        (
            "/etc/hosts".to_string(),
            "/etc/jsrt-no-such-file".to_string(),
        ),
    ];
    let list = pairs
        .iter()
        .map(|(a, b)| format!("[{a:?}, {b:?}]"))
        .collect::<Vec<_>>()
        .join(", ");
    rig.script(
        "app/oracle.js",
        &format!(
            "const probes = [\n\
               ['readText', (p) => rt.readText(p)],\n\
               ['media.put', (p) => rt.media.put(p)],\n\
               ['resolve', (p) => import.meta.resolve(p)],\n\
               ['import', (p) => import(p)],\n\
             ];\n\
             const outcome = async (probe, p) => {{\n\
               try {{ await probe(p); return 'allowed'; }}\n\
               catch (e) {{ return e.name + ': ' + String(e.message).replaceAll(p, '<asked>'); }}\n\
             }};\n\
             for (const [present, absent] of [{list}]) {{\n\
               for (const [name, probe] of probes) {{\n\
                 const a = await outcome(probe, present);\n\
                 const b = await outcome(probe, absent);\n\
                 rt.print((a === b ? 'same ' : 'DIFFERENT ') + name + ' ' + present + ' => ' + a + ' | ' + b + '\\n');\n\
               }}\n\
             }}\n"
        ),
    );
    // Relative readText and media paths resolve against the working
    // directory, imports against the module: run from app/ so that
    // `../outside/...` names the same files for both.
    let out = Command::new(JSRT)
        .args(["run", "oracle.js", "--manifest", arg(&manifest)])
        .current_dir(&app)
        .output()
        .unwrap_or_else(|e| panic!("spawn jsrt: {e}"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let seen = stdout(&out);
    assert_eq!(seen.lines().count(), pairs.len() * 4, "{seen}");
    for line in seen.lines() {
        assert!(line.starts_with("same "), "an existence oracle: {line}");
        assert!(!line.contains("allowed"), "{line}");
    }
}

#[test]
fn a_kv_store_that_cannot_open_does_not_name_its_path() {
    let rig = Rig::new();
    let app = rig.path("app");
    // The declared store is a directory: SQLite cannot open it, and
    // rusqlite's error carries the canonical path it tried.
    fs::create_dir_all(app.join("data/store.kv")).unwrap_or_else(|e| panic!("mkdir: {e}"));
    let manifest = rig.script(
        "app/m.manifest.json",
        "{ \"kv\": { \"path\": \"data/store.kv\" } }\n",
    );
    let script = rig.script(
        "app/kv.js",
        "for (const op of [() => rt.kv.get('k'), () => rt.kv.put('k', 'v')]) {\n\
           try { op(); rt.print('opened\\n'); }\n\
           catch (e) { rt.print(e.name + ': ' + e.message + '\\n'); }\n\
         }\n",
    );
    let out = rig.run_script(&script, &["--manifest", arg(&manifest)]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let seen = stdout(&out);
    assert_eq!(seen.lines().count(), 2, "{seen}");
    let root = fs::canonicalize(rig.dir.path()).unwrap_or_else(|e| panic!("canonicalize: {e}"));
    for line in seen.lines() {
        assert!(line.starts_with("HostError: kv open"), "{line}");
        assert!(
            !line.contains(root.to_string_lossy().as_ref()) && !line.contains("store.kv"),
            "a kv error names the store's path: {line}"
        );
    }
}

// ---- readText's 4 MiB cap holds for what is read, not what metadata says ---

#[test]
fn read_text_refuses_a_fifo_instead_of_reading_past_its_cap() {
    use std::os::unix::fs::OpenOptionsExt as _;
    let rig = Rig::new();
    let data = rig.path("data");
    fs::create_dir(&data).unwrap_or_else(|e| panic!("mkdir: {e}"));
    let fifo = data.join("pipe");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap_or_else(|e| panic!("mkfifo: {e}"));
    assert!(made.success(), "mkfifo failed");
    // A writer with 8 MiB for the pipe, twice the cap. Its size in the
    // metadata is 0, so a metadata-only cap lets the whole stream through.
    // Non-blocking, so a host that refuses to open the pipe leaves it
    // waiting for a reader that never comes, not hung.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let fifo = fifo.clone();
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let going = || !done.load(Ordering::SeqCst) && std::time::Instant::now() < deadline;
            let mut pipe = loop {
                match fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo)
                {
                    Ok(pipe) => break pipe,
                    Err(_) if going() => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(_) => return 0,
                }
            };
            let chunk = vec![b'f'; 64 * 1024];
            let mut sent = 0usize;
            while sent < 8 * 1024 * 1024 && going() {
                match pipe.write(&chunk) {
                    Ok(n) => sent += n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
            sent
        })
    };
    let script = rig.script(
        "fifo.js",
        "try { const t = await rt.readText('data/pipe'); rt.print('read ' + t.length + '\\n'); }\n\
         catch (e) { rt.print(e.name + ': ' + e.message + '\\n'); }\n",
    );
    let out = rig.run_script(&script, &["--read", arg(&data), "--timeout", "20000"]);
    done.store(true, Ordering::SeqCst);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "CapabilityError: read denied: data/pipe is not a regular file\n",
        "stderr: {}",
        stderr(&out)
    );
    let _ = writer.join();
}

// ---- A refused private name does not show the script its addresses --------

#[test]
fn a_refused_private_name_does_not_show_the_script_its_addresses() {
    let stub = CountingStub::start("private service");
    let rig = Rig::new();
    let script = rig.script(
        "private.js",
        &format!(
            "try {{ await rt.fetch('http://localhost:{port}/'); rt.print('fetched\\n'); }}\n\
             catch (e) {{ rt.print(e.name + ': ' + e.message + '\\n'); }}\n",
            port = stub.port
        ),
    );
    let out = rig.run_script(&script, &["--net", &format!("localhost:{}", stub.port)]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let seen = stdout(&out);
    assert!(
        seen.starts_with("CapabilityError: fetch denied: localhost")
            && seen.contains("not a public address"),
        "{seen}"
    );
    for addr in ["127.0.0.1", "::1"] {
        assert!(!seen.contains(addr), "the script sees {addr}: {seen}");
    }
    // The operator still gets the addresses, on stderr.
    assert!(
        stderr(&out).contains("127.0.0.1") || stderr(&out).contains("::1"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(stub.seen(), 0, "no connection was made");
}

// ---- A misspelt manifest key is an error, not a silent default -------------

#[test]
fn a_misspelt_manifest_key_is_an_error_naming_it() {
    let rig = Rig::new();
    let script = rig.script("ran.js", "rt.print('ran\\n');\n");
    // Each would have been ignored, leaving the default in its place: a
    // 64 MiB kv quota for `max_byte`, no read grant for `reads`.
    for (key, manifest) in [
        (
            "max_byte",
            "{ \"kv\": { \"path\": \"app.kv\", \"max_byte\": 1024 } }",
        ),
        ("reads", "{ \"reads\": [\".\"] }"),
        (
            "max_conections",
            "{ \"serve\": { \"host\": \"127.0.0.1\", \"port\": 9, \"max_conections\": 2 } }",
        ),
        (
            "maxbytes",
            "{ \"media\": { \"backend\": \"local\", \"root\": \".\", \"maxbytes\": 1 } }",
        ),
    ] {
        let path = rig.script("typo.manifest.json", manifest);
        let out = rig.run_script(&script, &["--manifest", arg(&path)]);
        assert_eq!(
            code(&out),
            8,
            "{key}: stdout {:?}, stderr: {}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            stderr(&out).contains(key),
            "{key}: stderr: {}",
            stderr(&out)
        );
        assert_eq!(stdout(&out), "", "{key}: the script must not run");
    }
}

// ---- A heap bomb inside a callback ends the run, with or without a timeout -

/// Run jsrt for at most `limit`. Returns the exit code (`None` when the
/// run was still going at `limit` and was killed, or died by a signal),
/// how long it took, and its stderr.
fn run_bounded(
    rig: &Rig,
    script: &Path,
    extra: &[&str],
    limit: std::time::Duration,
) -> (Option<i32>, std::time::Duration, String) {
    let mut args = vec!["run", arg(script)];
    args.extend_from_slice(extra);
    let started = std::time::Instant::now();
    let mut child = Command::new(JSRT)
        .args(&args)
        .current_dir(rig.dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn jsrt: {e}"));
    let mut finished = false;
    while started.elapsed() < limit {
        if matches!(child.try_wait(), Ok(Some(_))) {
            finished = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let took = started.elapsed();
    if !finished {
        let _ = child.kill();
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    let code = if finished { out.status.code() } else { None };
    (code, took, stderr(&out))
}

/// Allocates until the heap limit stops it.
const HEAP_BOMB: &str =
    "const hoard = []; while (true) { hoard.push(new Array(100000).fill({ x: hoard.length })); }";

#[test]
fn a_heap_bomb_inside_a_callback_ends_the_run_even_without_a_timeout() {
    let rig = Rig::new();
    // Each runs the bomb from the event loop rather than the module's top
    // level: there V8 terminates the callback, and the run must still end.
    let kinds = [
        (
            "setTimeout",
            format!("rt.setTimeout(() => {{ {HEAP_BOMB} }}, 0);\n"),
        ),
        (
            "queueMicrotask",
            format!("queueMicrotask(() => {{ {HEAP_BOMB} }});\n"),
        ),
        (
            "Promise.then",
            format!("Promise.resolve().then(() => {{ {HEAP_BOMB} }});\n"),
        ),
        (
            "after an await",
            format!("await rt.sleep(1);\n{HEAP_BOMB}\n"),
        ),
    ];
    for (kind, source) in kinds {
        let script = rig.script("callback-bomb.js", &source);
        let (code, took, err) = run_bounded(
            &rig,
            &script,
            &["--max-heap-mb", "64", "--timeout", "0"],
            std::time::Duration::from_secs(20),
        );
        assert_eq!(
            code,
            Some(6),
            "{kind}: still running or wrong ending after {took:?}; stderr: {}",
            err.lines().take(5).collect::<Vec<_>>().join("\n")
        );
        assert!(err.contains("heap limit"), "{kind}: stderr: {err}");
        assert!(
            took < std::time::Duration::from_secs(10),
            "{kind}: ended only after {took:?}"
        );
    }
}

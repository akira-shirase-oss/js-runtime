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

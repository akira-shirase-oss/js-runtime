//! jsrt — the CLI. Parses intent, hands the file to the host, maps the host's
//! error class to an exit code a script can branch on.

mod supervise;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use jsrt_host::{Capabilities, HostError, HostOptions};

/// jsrt — a purpose-built JavaScript host: V8 through deno_core, Rust owns
/// capabilities and the lifecycle.
#[derive(Parser)]
#[command(name = "jsrt", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The `run` contract, shared by `run` itself and `supervise` (which
/// re-serializes it for the worker it spawns).
#[derive(Args, Clone)]
struct RunArgs {
    /// The module file to execute.
    file: PathBuf,
    /// Wall-clock deadline in milliseconds (0 = no limit). Covers module
    /// load, evaluation, and every pending timer/fetch.
    #[arg(long, default_value_t = 10_000)]
    timeout: u64,
    /// Capability manifest (JSON): {"read": [dirs], "net": [hosts]}.
    /// Absent means deny-all.
    #[arg(long)]
    manifest: Option<PathBuf>,
    /// Extra read paths (joined with the manifest's).
    #[arg(long)]
    read: Vec<PathBuf>,
    /// Extra net destinations, host or host:port (joined with the manifest's).
    #[arg(long)]
    net: Vec<String>,
    /// Embedded KV store path — joins with the manifest's "kv" grant
    /// (a manifest line wins over the flag's default quota).
    #[arg(long)]
    kv: Option<PathBuf>,
    /// KV quota in MiB when granting via --kv (default 64).
    #[arg(long, default_value_t = 64)]
    kv_max_mb: u64,
    /// Local media store root — joins with the manifest's "media" grant.
    #[arg(long)]
    media_root: Option<PathBuf>,
    /// Per-upload media cap in MiB when granting via --media-root (default 256).
    #[arg(long, default_value_t = 256)]
    media_max_mb: u64,
    /// HTTP listener address, host:port (loopback IP literal only) —
    /// joins with the manifest's "serve" grant.
    #[arg(long)]
    serve: Option<String>,
    /// Per-fetch deadline in milliseconds (0 = the built-in 10s bound).
    #[arg(long, default_value_t = 0)]
    fetch_timeout: u64,
}

impl RunArgs {
    /// Serialize back to argv for `jsrt run` — the worker the supervisor
    /// spawns gets exactly what the operator asked for.
    fn to_argv(&self) -> Vec<String> {
        let mut v = vec![self.file.to_string_lossy().into_owned()];
        v.extend(["--timeout".into(), self.timeout.to_string()]);
        if let Some(m) = &self.manifest {
            v.extend(["--manifest".into(), m.to_string_lossy().into_owned()]);
        }
        for r in &self.read {
            v.extend(["--read".into(), r.to_string_lossy().into_owned()]);
        }
        for n in &self.net {
            v.extend(["--net".into(), n.clone()]);
        }
        if let Some(kv) = &self.kv {
            v.extend(["--kv".into(), kv.to_string_lossy().into_owned()]);
        }
        v.extend(["--kv-max-mb".into(), self.kv_max_mb.to_string()]);
        if let Some(root) = &self.media_root {
            v.extend(["--media-root".into(), root.to_string_lossy().into_owned()]);
        }
        v.extend(["--media-max-mb".into(), self.media_max_mb.to_string()]);
        if let Some(serve) = &self.serve {
            v.extend(["--serve".into(), serve.clone()]);
        }
        v.extend(["--fetch-timeout".into(), self.fetch_timeout.to_string()]);
        v
    }
}

#[derive(Subcommand)]
enum Command {
    /// Run a JavaScript module to completion.
    Run(RunArgs),
    /// Run a module in a supervised worker process — CPU seconds by rlimit,
    /// resident memory by footprint polling, a wall backstop, and a JSON
    /// report of how it ended.
    Supervise {
        #[command(flatten)]
        run: RunArgs,
        /// Resident-footprint ceiling in MiB; over it the worker dies.
        #[arg(long)]
        max_rss_mb: Option<u64>,
        /// CPU-seconds ceiling via RLIMIT_CPU — the kernel delivers SIGXCPU.
        #[arg(long)]
        max_cpu: Option<u64>,
        /// Wall-clock backstop in milliseconds for the whole supervised run
        /// (0 = rely on the worker's own --timeout).
        #[arg(long, default_value_t = 0)]
        wall_ms: u64,
        /// Grace between SIGTERM and SIGKILL, in milliseconds.
        #[arg(long, default_value_t = 500)]
        kill_grace_ms: u64,
        /// Append the JSON run report to this file instead of stdout.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Byte cap for the report file before it rotates to `<name>.1`
        /// with a dropped-record meta-line (default 512 KiB).
        #[arg(long, default_value_t = 512)]
        report_max_kb: u64,
    },
}

fn exit_code(err: &HostError) -> i32 {
    err.exit_code()
}

/// Single-thread executor: `JsRuntime` is `!Send`, so every run must stay on
/// the thread that created it.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(run) => {
            let result = execute(run).await;
            if let Err(err) = result {
                eprintln!("jsrt: {err}");
                std::process::exit(exit_code(&err));
            }
        }
        Command::Supervise {
            run,
            max_rss_mb,
            max_cpu,
            wall_ms,
            kill_grace_ms,
            report,
            report_max_kb,
        } => {
            let plan = supervise::SupervisePlan {
                worker_args: run.to_argv(),
                max_rss_bytes: max_rss_mb.map(|mb| mb * 1024 * 1024),
                max_cpu_secs: max_cpu,
                wall: (wall_ms > 0).then(|| Duration::from_millis(wall_ms)),
                kill_grace: Duration::from_millis(kill_grace_ms),
                report_path: report,
                report_max_bytes: report_max_kb * 1024,
            };
            match supervise::supervise(&plan) {
                Ok(code) => std::process::exit(code),
                Err(err) => {
                    eprintln!("jsrt: {err}");
                    std::process::exit(exit_code(&err));
                }
            }
        }
    }
}

async fn execute(run: RunArgs) -> Result<(), HostError> {
    let capabilities = match &run.manifest {
        Some(path) => Capabilities::from_manifest_file(path)?,
        None => Capabilities::deny_all(),
    };
    let capabilities = merge_cli_grants(
        capabilities,
        CliGrants {
            read: run.read,
            net: run.net,
            kv: run.kv,
            kv_max_mb: run.kv_max_mb,
            media_root: run.media_root,
            media_max_mb: run.media_max_mb,
            serve: run.serve,
        },
    )?;
    let options = HostOptions {
        capabilities,
        timeout: (run.timeout > 0).then(|| Duration::from_millis(run.timeout)),
        // A parked event loop can't be interrupted — if termination
        // doesn't unwind in 2s the process exits with the same code.
        terminate_grace: (run.timeout > 0).then(|| Duration::from_secs(2)),
        fetch_deadline: (run.fetch_timeout > 0).then(|| Duration::from_millis(run.fetch_timeout)),
    };
    jsrt_host::run_file(&run.file, &options).await
}

/// The CLI's grant flags, grouped — additive only, they can widen a
/// manifest, never weaken it.
struct CliGrants {
    read: Vec<PathBuf>,
    net: Vec<String>,
    kv: Option<PathBuf>,
    kv_max_mb: u64,
    media_root: Option<PathBuf>,
    media_max_mb: u64,
    serve: Option<String>,
}

fn merge_cli_grants(caps: Capabilities, flags: CliGrants) -> Result<Capabilities, HostError> {
    if flags.read.is_empty()
        && flags.net.is_empty()
        && flags.kv.is_none()
        && flags.media_root.is_none()
        && flags.serve.is_none()
    {
        return Ok(caps);
    }
    let mut merged = caps.into_grants();
    for dir in &flags.read {
        let resolved = std::fs::canonicalize(dir).map_err(|e| {
            HostError::Host(format!("--read {} does not resolve: {e}", dir.display()))
        })?;
        merged.read_paths.push(resolved);
    }
    merged.net_hosts.extend(flags.net);
    if let Some(path) = flags.kv {
        // Only extend, never weaken: a manifest "kv" line keeps its own
        // quota over the flag's default.
        if merged.kv.is_none() {
            let parent = path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            let canonical = std::fs::canonicalize(&parent).map_err(|e| {
                HostError::Host(format!(
                    "--kv {} parent does not resolve: {e}",
                    path.display()
                ))
            })?;
            let file_name = path.file_name().map(|n| n.to_os_string()).ok_or_else(|| {
                HostError::Host(format!("--kv {} has no file name", path.display()))
            })?;
            merged.kv = Some(jsrt_host::capability::KvGrant {
                path: canonical.join(file_name),
                max_bytes: flags.kv_max_mb * 1024 * 1024,
            });
        }
    }
    if let Some(root) = flags.media_root {
        if merged.media.is_none() {
            let canonical = std::fs::canonicalize(&root).map_err(|e| {
                HostError::Host(format!(
                    "--media-root {} does not resolve: {e}",
                    root.display()
                ))
            })?;
            if !canonical.is_dir() {
                return Err(HostError::Host(format!(
                    "--media-root {} is not a directory",
                    canonical.display()
                )));
            }
            merged.media = Some(jsrt_host::capability::MediaGrant {
                root: canonical,
                max_bytes: flags.media_max_mb * 1024 * 1024,
            });
        }
    }
    if let Some(addr) = flags.serve {
        if merged.serve.is_none() {
            let sock: std::net::SocketAddr = addr.parse().map_err(|_| {
                HostError::Host(format!("--serve {addr} is not a host:port IP literal"))
            })?;
            if !sock.ip().is_loopback() {
                return Err(HostError::Host(format!(
                    "--serve {addr} is not loopback — v1 binds loopback only"
                )));
            }
            merged.serve = Some(jsrt_host::capability::ServeGrant {
                addr: sock,
                max_body: 8 * 1024 * 1024,
            });
        }
    }
    Ok(Capabilities::from_grants(merged))
}

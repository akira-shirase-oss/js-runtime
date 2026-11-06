//! jsrt — the CLI. Parses intent, hands the file to the host, maps the host's
//! error class to an exit code a script can branch on.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use jsrt_host::{Capabilities, HostError, HostOptions};

/// jsrt — a purpose-built JavaScript host: V8 through deno_core, Rust owns
/// capabilities and the lifecycle.
#[derive(Parser)]
#[command(name = "jsrt", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a JavaScript module to completion.
    Run {
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
        /// Per-fetch deadline in milliseconds (0 = the built-in 10s bound).
        #[arg(long, default_value_t = 0)]
        fetch_timeout: u64,
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
    let result = match cli.command {
        Command::Run {
            file,
            timeout,
            manifest,
            read,
            net,
            fetch_timeout,
        } => {
            let capabilities = match &manifest {
                Some(path) => match Capabilities::from_manifest_file(path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("jsrt: {e}");
                        std::process::exit(exit_code(&e));
                    }
                },
                None => Capabilities::deny_all(),
            };
            let capabilities = merge_cli_grants(capabilities, read, net);
            let options = HostOptions {
                capabilities,
                timeout: (timeout > 0).then(|| Duration::from_millis(timeout)),
                // A parked event loop can't be interrupted — if termination
                // doesn't unwind in 2s the process exits with the same code.
                terminate_grace: (timeout > 0).then(|| Duration::from_secs(2)),
                fetch_deadline: (fetch_timeout > 0).then(|| Duration::from_millis(fetch_timeout)),
            };
            jsrt_host::run_file(&file, &options).await
        }
    };
    if let Err(err) = result {
        eprintln!("jsrt: {err}");
        std::process::exit(exit_code(&err));
    }
}

/// `--read`/`--net` flags extend the manifest — grants are additive, never
/// subtractive, so CLI flags can only widen a manifest, not weaken it.
fn merge_cli_grants(caps: Capabilities, read: Vec<PathBuf>, net: Vec<String>) -> Capabilities {
    if read.is_empty() && net.is_empty() {
        return caps;
    }
    let mut merged = caps.into_grants();
    for dir in &read {
        match std::fs::canonicalize(dir) {
            Ok(resolved) => merged.read_paths.push(resolved),
            Err(e) => {
                eprintln!("jsrt: --read {} does not resolve: {e}", dir.display());
                std::process::exit(HostError::Host(format!("--read {e}")).exit_code());
            }
        }
    }
    merged.net_hosts.extend(net);
    Capabilities::from_grants(merged)
}

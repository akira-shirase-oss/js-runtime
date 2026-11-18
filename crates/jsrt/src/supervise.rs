//! `jsrt supervise` — the hard boundary an in-process watchdog can never be.
//!
//! `terminate_execution` can stop JavaScript; it cannot stop allocation, and
//! it cannot outlive the isolate it watches. The supervisor is the same
//! trick every production embedder uses: the runtime lives in a *child
//! process*, and the parent enforces what the child cannot enforce on
//! itself — CPU seconds via `setrlimit` (applied post-fork, before exec, so
//! the child cannot decline), resident memory by polling `proc_pid_rusage`,
//! and a wall-clock backstop over the whole thing.
//!
//! What the supervisor guarantees the in-process watchdog cannot:
//! - a memory bomb is an OS problem — the supervisor kills it and reports
//!   which budget tripped, instead of the host aborting itself;
//! - the supervisor survives whatever the worker does — SIGSEGV, SIGABRT,
//!   an unhandled path in deno_core — and still emits a classified report.
//!
//! Honest limits, written down: RSS polling is a bound, not a brick wall —
//! worst-case overage is (allocation rate × poll interval). `RLIMIT_AS` is
//! deliberately not used: V8 reserves a multi-GB pointer cage at startup,
//! so an address-space limit sized for RSS would prevent the engine from
//! booting. The memory budget is therefore *enforced on observed
//! footprint*, which is the only honest bound a V8 host can offer without
//! kernel accounting.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

use jsrt_host::HostError;

/// How often the supervisor samples the child's resident footprint.
/// Worst-case overage is roughly `peak allocation rate × interval`.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Signals the supervisor considers "the budget did its job" — anything
/// else is an unexpected death the report must name, not launder.
const KILLED_BY_CPU: &str = "cpu-budget";
const KILLED_BY_RSS: &str = "rss-budget";
const KILLED_BY_WALL: &str = "wall-clock";

/// Set by the signal handler when the operator (or a process manager)
/// wants the supervised run down. The supervisor forwards it to the
/// worker — a supervisor that can't be stopped isn't supervision.
static OPERATOR_TERM: AtomicBool = AtomicBool::new(false);

extern "C" fn on_operator_term(_sig: libc::c_int) {
    // Atomic store is async-signal-safe — the handler only raises a flag.
    OPERATOR_TERM.store(true, Ordering::SeqCst);
}

pub struct SupervisePlan {
    /// Arguments to pass to `jsrt run` verbatim (already serialized).
    pub worker_args: Vec<String>,
    /// Resident-footprint budget; over it, the worker dies.
    pub max_rss_bytes: Option<u64>,
    /// `RLIMIT_CPU` seconds; the kernel delivers SIGXCPU when exceeded.
    pub max_cpu_secs: Option<u64>,
    /// Optional wall-clock backstop for the whole supervised run.
    pub wall: Option<Duration>,
    /// Grace between SIGTERM and SIGKILL.
    pub kill_grace: Duration,
    /// Where the JSON run report goes — a file, or stdout.
    pub report_path: Option<PathBuf>,
    /// Byte cap for the report file before it rotates.
    pub report_max_bytes: u64,
    /// Bytes of worker stdout, and separately stderr, passed through.
    /// With `output_rate_bps` this is the burst allowance, refilled at
    /// that rate; at rate 0 it is a lifetime budget.
    pub max_output_bytes: u64,
    /// Sustained pass-through in bytes/s once the burst is spent
    /// (0 = lifetime cap only — a long-lived worker goes quiet at it).
    pub output_rate_bps: u64,
    /// Also prefix worker stdout lines with the run id. stderr lines are
    /// always prefixed; stdout is a data pipe and stays byte-exact unless
    /// the operator asks.
    pub prefix_stdout: bool,
    /// Literals masked out of forwarded worker output (and the re-shown
    /// tail). They never appear in argv, logs or the report — the report
    /// carries only their count and masked bytes.
    pub redact_literals: Vec<Vec<u8>>,
    /// Pass worker bytes untouched — no run-id prefix and no terminal
    /// neutralising. For trusted workers; configured redaction still
    /// applies.
    pub output_raw: bool,
}

/// The last bytes of each capped stream, kept and shown at exit — where a
/// worker's cause of death (the host's final error line) usually is.
const OUTPUT_TAIL_BYTES: usize = 4096;

/// What one worker stream did: bytes passed through, bytes dropped past
/// the cap, how many of the dropped bytes were re-shown as the tail, and
/// how many emitted bytes were masked as secrets.
#[derive(Debug, Default, Clone, Copy)]
struct Forwarded {
    written: u64,
    dropped: u64,
    tail: u64,
    redacted: u64,
}

/// What one worker stream's forwarder does with the bytes the budget
/// lets through: pass them untouched, or mark each line with its run id
/// so a shared log says which run produced it.
struct ForwardCfg {
    cap: u64,
    rate_bps: u64,
    /// Line prefix marking the stream's run; `None` passes bytes raw.
    prefix: Option<String>,
    /// Literals masked out of the emitted stream.
    secrets: Vec<Vec<u8>>,
    /// The destination is a terminal: render control bytes visibly so the
    /// worker cannot drive the operator's terminal.
    sanitise: bool,
}

const REDACTED: &[u8] = b"[redacted]";

/// Renders a worker's output safe for a terminal, UTF-8 aware: C0
/// controls except `\n` and `\t`, DEL, and the C1 code points become
/// `^X` / `M-^x` (a UTF-8 terminal receives U+0080..U+009F as C2 80..9F).
/// Every other valid scalar passes byte-for-byte — the 0x80..0x9F range
/// is ordinary continuation bytes in text like "café", "€" or "😀".
/// Invalid or lone bytes render as `\xNN`: a lone 0x9B is still CSI in
/// 8-bit terminal modes. An incomplete trailing sequence (at most 3
/// bytes) is held for the next chunk — same shape as the redactor's
/// hold-back — and flushed rendered at EOF.
struct Neutraliser {
    hold: Vec<u8>,
}

impl Neutraliser {
    fn new() -> Self {
        Self { hold: Vec::new() }
    }

    fn process(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.hold.extend_from_slice(chunk);
        let hay = std::mem::take(&mut self.hold);
        let mut i = 0;
        while i < hay.len() {
            let b = hay[i];
            match b {
                b'\n' | b'\t' => {
                    out.push(b);
                    i += 1;
                }
                0x00..=0x08 | 0x0b..=0x1f => {
                    out.push(b'^');
                    out.push(b + 0x40);
                    i += 1;
                }
                0x7f => {
                    out.extend_from_slice(b"^?");
                    i += 1;
                }
                0x20..=0x7e => {
                    out.push(b);
                    i += 1;
                }
                _ => {
                    let len = utf8_len(b);
                    if len == 0 {
                        push_hex(out, b);
                        i += 1;
                        continue;
                    }
                    if hay.len() - i < len {
                        if utf8_valid_prefix(&hay[i..]) {
                            break;
                        }
                        push_hex(out, b);
                        i += 1;
                        continue;
                    }
                    let seq = &hay[i..i + len];
                    if !utf8_valid_seq(seq) {
                        push_hex(out, b);
                        i += 1;
                        continue;
                    }
                    if len == 2 && b == 0xc2 && (0x80..=0x9f).contains(&seq[1]) {
                        out.extend_from_slice(b"M-^");
                        out.push(seq[1] - 0x80 + 0x40);
                    } else {
                        out.extend_from_slice(seq);
                    }
                    i += len;
                }
            }
        }
        self.hold.extend_from_slice(&hay[i..]);
    }

    /// The stream ended: a held partial sequence can never complete —
    /// show its bytes rather than drop or emit them raw.
    fn finish(&mut self, out: &mut Vec<u8>) {
        for &b in std::mem::take(&mut self.hold).iter() {
            push_hex(out, b);
        }
    }
}

fn push_hex(out: &mut Vec<u8>, b: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.extend_from_slice(b"\\x");
    out.push(HEX[(b >> 4) as usize]);
    out.push(HEX[(b & 0xf) as usize]);
}

/// Length of the sequence a lead byte opens, or 0 for bytes that cannot
/// lead one (lone continuations, overlong C0/C1, starts above U+10FFFF).
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

/// The constraint on the byte right after the lead — the only position
/// whose range varies (overlong, surrogate and >U+10FFFF screens).
fn second_byte_ok(lead: u8, b: u8) -> bool {
    match lead {
        0xe0 => (0xa0..=0xbf).contains(&b),
        0xed => (0x80..=0x9f).contains(&b),
        0xf0 => (0x90..=0xbf).contains(&b),
        0xf4 => (0x80..=0x8f).contains(&b),
        _ => (0x80..=0xbf).contains(&b),
    }
}

/// `part`, shorter than a full sequence, could still complete into a
/// valid one: every byte present satisfies its position's range.
fn utf8_valid_prefix(part: &[u8]) -> bool {
    utf8_len(part[0]) > 0
        && part.get(1).map_or(true, |&b| second_byte_ok(part[0], b))
        && part.iter().skip(2).all(|&b| (0x80..=0xbf).contains(&b))
}

/// `seq` is a complete, strictly valid UTF-8 sequence.
fn utf8_valid_seq(seq: &[u8]) -> bool {
    second_byte_ok(seq[0], seq[1]) && seq[2..].iter().all(|&b| (0x80..=0xbf).contains(&b))
}

/// Masks configured literals in the forwarded stream, across chunk
/// boundaries: the last `max_len - 1` unemitted bytes are held back so a
/// secret split over two reads is still caught. `redacted` counts the
/// original bytes masked — the marker is framing, like the line prefix.
struct Redactor {
    secrets: Vec<Vec<u8>>,
    /// Secret indices by their first byte: a position only scans the
    /// literals it could begin. Without it every byte paid a
    /// `starts_with` per secret — ~2.5 G compares over 10 MiB of output
    /// at the 256-secret bound, enough to stall the worker past its
    /// timeout. Worst case (all secrets sharing a first byte) stays
    /// bounded by the 256 cap.
    buckets: Vec<Vec<usize>>,
    hold: Vec<u8>,
    max_len: usize,
    redacted: u64,
}

impl Redactor {
    fn new(secrets: Vec<Vec<u8>>) -> Self {
        let max_len = secrets.iter().map(|s| s.len()).max().unwrap_or(0);
        let mut buckets = vec![Vec::new(); 256];
        for (i, s) in secrets.iter().enumerate() {
            if let Some(&first) = s.first() {
                buckets[first as usize].push(i);
            }
        }
        Self {
            secrets,
            buckets,
            hold: Vec::new(),
            max_len,
            redacted: 0,
        }
    }

    fn active(&self) -> bool {
        self.max_len > 0
    }

    /// Append `chunk`; emit into `out` everything that cannot still open
    /// a secret, with matches replaced by "[redacted]".
    fn process(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        if !self.active() {
            out.extend_from_slice(chunk);
            return;
        }
        let mut hay = std::mem::take(&mut self.hold);
        hay.extend_from_slice(chunk);
        let mut pos = 0;
        while let Some(start) = self.next_match(&hay, pos, false) {
            out.extend_from_slice(&hay[pos..start]);
            let end = self.mask_union(&hay, start, false);
            out.extend_from_slice(REDACTED);
            self.redacted += (end - start) as u64;
            pos = end;
        }
        // Anything in the last max_len-1 bytes could be the start of a
        // secret the next chunk completes — hold it, emit the rest.
        let hold_start = pos.max(hay.len().saturating_sub(self.max_len - 1));
        out.extend_from_slice(&hay[pos..hold_start]);
        self.hold = hay[hold_start..].to_vec();
    }

    /// The stream ended: emit the held tail, masking a dangling secret
    /// prefix too — showing it would reveal all but the secret's end.
    fn finish(&mut self, out: &mut Vec<u8>) {
        let hay = std::mem::take(&mut self.hold);
        let mut pos = 0;
        while let Some(start) = self.next_match(&hay, pos, true) {
            out.extend_from_slice(&hay[pos..start]);
            let end = self.mask_union(&hay, start, true);
            out.extend_from_slice(REDACTED);
            self.redacted += (end - start) as u64;
            pos = end;
        }
        out.extend_from_slice(&hay[pos..]);
    }

    /// First position at or after `from` where a secret starts.
    fn next_match(&self, hay: &[u8], from: usize, final_chunk: bool) -> Option<usize> {
        (from..hay.len()).find(|&i| self.match_at(hay, i, final_chunk).is_some())
    }

    /// Longest match starting at `i` as bytes masked, or a proper-prefix
    /// match at the very tail when `final_chunk` — mid-stream it may
    /// still complete or dissolve.
    fn match_at(&self, hay: &[u8], i: usize, final_chunk: bool) -> Option<usize> {
        let mut best: Option<usize> = None;
        for &si in &self.buckets[hay[i] as usize] {
            let s = &self.secrets[si];
            let len = if hay[i..].starts_with(s) {
                s.len()
            } else if final_chunk && hay.len() - i < s.len() && s.starts_with(&hay[i..]) {
                hay.len() - i
            } else {
                continue;
            };
            best = Some(best.map_or(len, |b| b.max(len)));
        }
        best
    }

    /// End of the masked union opened at `start`: a match that begins
    /// inside the masked span extends it. Masking only the first match
    /// would show the tail of any secret it overlaps — "1234" of
    /// "12345678" leaves "5678" of "345678" visible.
    fn mask_union(&self, hay: &[u8], start: usize, final_chunk: bool) -> usize {
        let mut end = start + self.match_at(hay, start, final_chunk).unwrap_or(0);
        let mut j = start + 1;
        while j < end && j < hay.len() {
            if let Some(l) = self.match_at(hay, j, final_chunk) {
                end = end.max(j + l);
            }
            j += 1;
        }
        end
    }
}

/// Write `bytes` to `to` with `prefix` at the start of every line,
/// tracking `at_line_start` across calls so a line split over several
/// reads stays byte-exact and still goes out live. Prefix bytes are
/// framing — they are not counted against the worker's output budget.
fn write_prefixed(
    to: &mut impl Write,
    prefix: &str,
    at_line_start: &mut bool,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut rest = bytes;
    while !rest.is_empty() {
        if *at_line_start {
            to.write_all(prefix.as_bytes())?;
            *at_line_start = false;
        }
        match rest.iter().position(|b| *b == b'\n') {
            Some(i) => {
                to.write_all(&rest[..=i])?;
                *at_line_start = true;
                rest = &rest[i + 1..];
            }
            None => {
                to.write_all(rest)?;
                rest = &[];
            }
        }
    }
    to.flush()
}

/// Copy `from` to `to` until EOF, passing what a token bucket allows and
/// counting the rest. `cap` is the burst (the bucket starts full) and
/// `rate_bps` refills it; at rate 0 that degenerates to the old lifetime
/// cap. The bucket's capacity floors at one read buffer so a 0-burst
/// plus a rate still lets a trickle through. The forwarder keeps reading
/// past any drop — a worker blocked on a full pipe would stall instead
/// of being limited — so the bound is on what reaches the operator, not
/// on what the worker may try to write.
///
/// The moment output is first dropped, a notice goes to `notice` (the
/// supervisor's stderr), so a long-running worker's operator learns it
/// then, not at exit. The last `OUTPUT_TAIL_BYTES` of the dropped output
/// are kept and written at EOF, starting at a line boundary when there is
/// one, so the final lines survive.
fn forward(
    name: &str,
    mut from: impl Read,
    mut to: impl Write,
    mut notice: impl Write,
    cfg: ForwardCfg,
) -> Forwarded {
    let cap = cfg.cap;
    let rate_bps = cfg.rate_bps;
    let mut done = Forwarded::default();
    let mut tail: std::collections::VecDeque<u8> =
        std::collections::VecDeque::with_capacity(OUTPUT_TAIL_BYTES);
    let mut buf = [0u8; 8192];
    let mut at_line_start = true;
    let rate = rate_bps as f64;
    let capacity = cap.max(buf.len() as u64) as f64;
    let mut tokens = cap as f64;
    let mut last_refill = Instant::now();
    let mut redactor = Redactor::new(cfg.secrets);
    let mut neutraliser = Neutraliser::new();
    let mut emit = Vec::new();
    let mut emit2 = Vec::new();
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if rate > 0.0 {
            // Quiet time accrues allowance — that is what lets a burst
            // form again after the bucket was spent.
            let now = Instant::now();
            tokens = (tokens + now.duration_since(last_refill).as_secs_f64() * rate).min(capacity);
            last_refill = now;
        }
        let keep = n.min(tokens as usize);
        tokens -= keep as f64;
        if keep > 0 {
            emit.clear();
            redactor.process(&buf[..keep], &mut emit);
            let payload: &[u8] = if cfg.sanitise {
                emit2.clear();
                neutraliser.process(&emit, &mut emit2);
                &emit2
            } else {
                &emit
            };
            let wrote = match &cfg.prefix {
                Some(prefix) => write_prefixed(&mut to, prefix, &mut at_line_start, payload),
                None => to.write_all(payload).and_then(|()| to.flush()),
            };
            if wrote.is_ok() {
                done.written += keep as u64;
            } else {
                done.dropped += keep as u64;
            }
        }
        let over = &buf[keep..n];
        if !over.is_empty() {
            if done.dropped == 0 {
                if rate_bps > 0 {
                    let _ = writeln!(
                        notice,
                        "\njsrt: supervise: worker {name} exceeded its output budget \
                         ({cap}-byte burst + {rate_bps} bytes/s); excess output is dropped \
                         and counted, its last {OUTPUT_TAIL_BYTES} bytes are shown at exit"
                    );
                } else {
                    let _ = writeln!(
                        notice,
                        "\njsrt: supervise: worker {name} reached the {cap}-byte cap; further \
                         output is dropped and counted, its last {OUTPUT_TAIL_BYTES} bytes \
                         are shown at exit"
                    );
                }
                let _ = notice.flush();
            }
            done.dropped += over.len() as u64;
            for &b in over {
                if tail.len() == OUTPUT_TAIL_BYTES {
                    tail.pop_front();
                }
                tail.push_back(b);
            }
        }
    }
    // EOF: flush what the redactor held back — the stream is over, so a
    // dangling secret prefix is masked rather than shown.
    emit.clear();
    redactor.finish(&mut emit);
    if !emit.is_empty() {
        let payload: &[u8] = if cfg.sanitise {
            emit2.clear();
            neutraliser.process(&emit, &mut emit2);
            &emit2
        } else {
            &emit
        };
        let _ = match &cfg.prefix {
            Some(prefix) => write_prefixed(&mut to, prefix, &mut at_line_start, payload),
            None => to.write_all(payload).and_then(|()| to.flush()),
        };
    }
    // A scalar truncated at EOF renders as its \xNN bytes. Never fed
    // when the sink is not a terminal, so its hold is empty and this
    // writes nothing.
    emit2.clear();
    neutraliser.finish(&mut emit2);
    if !emit2.is_empty() {
        let _ = match &cfg.prefix {
            Some(prefix) => write_prefixed(&mut to, prefix, &mut at_line_start, &emit2),
            None => to.write_all(&emit2).and_then(|()| to.flush()),
        };
    }
    if !tail.is_empty() {
        let mut bytes: Vec<u8> = tail.into_iter().collect();
        // Start at a line boundary if the window cut a line — unless the
        // only newline ends the window, which would leave nothing.
        if done.dropped > OUTPUT_TAIL_BYTES as u64 {
            if let Some(i) = bytes.iter().position(|&b| b == b'\n') {
                if i + 1 < bytes.len() {
                    bytes.drain(..=i);
                }
            }
        }
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        // The re-shown tail gets the same masking — a dropped secret
        // must not resurface at exit — and the same terminal rendering.
        emit.clear();
        redactor.process(&bytes, &mut emit);
        redactor.finish(&mut emit);
        let bytes = if cfg.sanitise {
            emit2.clear();
            neutraliser.process(&emit, &mut emit2);
            neutraliser.finish(&mut emit2);
            std::mem::take(&mut emit2)
        } else {
            emit
        };
        let header = format!(
            "\n--- jsrt: supervise: last {} of the {} bytes of worker {name} past the cap ---\n",
            bytes.len(),
            done.dropped
        );
        if to
            .write_all(header.as_bytes())
            .and_then(|()| to.write_all(&bytes))
            .and_then(|()| to.flush())
            .is_ok()
        {
            done.tail = bytes.len() as u64;
        }
    }
    done.redacted = redactor.redacted;
    done
}

/// Execute the supervision plan. Returns the process exit code jsrt
/// should use — the worker's class, or `terminated` (6) when a budget
/// did the killing.
pub fn supervise(plan: &SupervisePlan) -> Result<i32, HostError> {
    // A supervisor that can't be stopped isn't supervision: SIGTERM/SIGINT
    // to the supervisor forwards to the worker through the same
    // SIGTERM→SIGKILL path the budgets use, recorded as operator-sigterm.
    // The handler is armed *before* the worker exists — a TERM that lands
    // while the worker is still spawning sets the flag and forwards the
    // moment the poll loop starts. The only uncovered window left is the
    // process's own startup, which no design removes.
    OPERATOR_TERM.store(false, Ordering::SeqCst);
    // SAFETY: `on_operator_term` is an `extern "C" fn(c_int)`, the handler
    // type `signal` expects, and it only stores to an AtomicBool, which is
    // async-signal-safe. Replacing the default SIGTERM/SIGINT disposition
    // breaks no invariant the rest of the program relies on.
    unsafe {
        if libc::signal(
            libc::SIGTERM,
            on_operator_term as *const () as libc::sighandler_t,
        ) == libc::SIG_ERR
        {
            return Err(HostError::Host(format!(
                "supervise: signal(SIGTERM): {}",
                std::io::Error::last_os_error()
            )));
        }
        if libc::signal(
            libc::SIGINT,
            on_operator_term as *const () as libc::sighandler_t,
        ) == libc::SIG_ERR
        {
            return Err(HostError::Host(format!(
                "supervise: signal(SIGINT): {}",
                std::io::Error::last_os_error()
            )));
        }
    }

    let started = Instant::now();
    let run_id = format!(
        "{}x{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        std::process::id()
    );

    let exe = std::env::current_exe()
        .map_err(|e| HostError::Host(format!("supervise: own binary: {e}")))?;

    let mut cmd = Command::new(&exe);
    cmd.arg("run").args(&plan.worker_args);
    cmd.stdin(Stdio::null());
    // Piped, not inherited: a flooding worker is limited here, per stream,
    // before its output reaches the operator's terminal, log or disk.
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // rlimits land post-fork pre-exec: the budget is set by the parent on
    // the child, so the child cannot decline it. Only CPU is rlimit-able —
    // RLIMIT_AS would stop V8's pointer-cage reservation before boot.
    if let Some(cpu) = plan.max_cpu_secs {
        // SAFETY: the closure runs in the forked child before exec, where
        // only async-signal-safe calls are allowed. It builds an `rlimit`
        // on the stack and calls `setrlimit` (async-signal-safe); the error
        // path wraps errno without allocating. No locks, no allocation.
        unsafe {
            cmd.pre_exec(move || {
                let lim = libc::rlimit {
                    rlim_cur: cpu,
                    rlim_max: libc::RLIM_INFINITY,
                };
                if libc::setrlimit(libc::RLIMIT_CPU, &lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| HostError::Host(format!("supervise: spawn {}: {e}", exe.display())))?;
    let pid = child.id() as libc::pid_t;
    // The forwarders end at EOF, which comes when the worker exits (scripts
    // cannot start processes that would inherit the pipes).
    let cap = plan.max_output_bytes;
    let rate = plan.output_rate_bps;
    // Worker stderr always carries its run id — that is where the host's
    // error lines land; stdout is a data pipe and gets the marker only
    // when asked.
    let line_prefix = format!("[run {run_id}] ");
    // A terminal sink gets neutralised control bytes — a worker must not
    // drive the operator's terminal. A pipe keeps the bytes as they are:
    // downstream consumers want the worker's output, not a rewrite.
    use std::io::IsTerminal;
    let stdout_tty = std::io::stdout().is_terminal();
    let stderr_tty = std::io::stderr().is_terminal();
    let out_cfg = ForwardCfg {
        cap,
        rate_bps: rate,
        prefix: (plan.prefix_stdout && !plan.output_raw).then(|| line_prefix.clone()),
        secrets: plan.redact_literals.clone(),
        sanitise: stdout_tty && !plan.output_raw,
    };
    let err_cfg = ForwardCfg {
        cap,
        rate_bps: rate,
        prefix: (!plan.output_raw).then_some(line_prefix),
        secrets: plan.redact_literals.clone(),
        sanitise: stderr_tty && !plan.output_raw,
    };
    let out_forwarder = child.stdout.take().map(|out| {
        std::thread::spawn(move || {
            forward("stdout", out, std::io::stdout(), std::io::stderr(), out_cfg)
        })
    });
    let err_forwarder = child.stderr.take().map(|err| {
        std::thread::spawn(move || {
            forward("stderr", err, std::io::stderr(), std::io::stderr(), err_cfg)
        })
    });
    // The handle stays alive purely so the child stays ours until reaped —
    // dropping it would orphan nothing (Child::drop doesn't reap), but the
    // pid is only safe to use while we hold the handle.
    let _child_guard = child;

    // Operator-facing lifecycle line: the supervisor is armed and the
    // worker exists — the point at which SIGTERM/SIGINT forward cleanly.
    eprintln!("jsrt: supervise: worker {pid} running, signals forwarded");

    let mut peak_rss: u64 = 0;
    let mut killed_by: Option<&'static str> = None;
    // Two-stage kill, same shape as the in-process watchdog one level down:
    // SIGTERM, then SIGKILL after grace. `reap()` in this loop is the only
    // reaper — a second one anywhere would hit ECHILD on an already-reaped
    // zombie.
    let mut sigterm_at: Option<Instant> = None;
    let mut sigkill_sent = false;

    let (status, usage) = loop {
        match reap(pid) {
            Reap::Exited(s, ru) => break (s, ru),
            Reap::Running => {}
            Reap::Error(e) => {
                return Err(HostError::Host(format!("supervise: wait4: {e}")));
            }
        }

        if let Some(rss) = resident_bytes(pid) {
            peak_rss = peak_rss.max(rss);
            if let Some(budget) = plan.max_rss_bytes {
                if rss > budget && sigterm_at.is_none() {
                    killed_by = Some(KILLED_BY_RSS);
                    // SAFETY: `kill` takes plain integers and touches none of our
                    // memory. `pid` is our own child, not reaped yet (wait4
                    // reported it running this iteration), so even if it has
                    // exited it is a zombie and its pid cannot be reused.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    sigterm_at = Some(Instant::now());
                }
            }
        }

        if let Some(wall) = plan.wall {
            if started.elapsed() > wall && sigterm_at.is_none() {
                killed_by = Some(KILLED_BY_WALL);
                // SAFETY: as for the RSS kill above: plain integers, and
                // `pid` is our unreaped child.
                unsafe { libc::kill(pid, libc::SIGTERM) };
                sigterm_at = Some(Instant::now());
            }
        }

        // The operator asked the supervisor to stop — forward through the
        // same path the budgets take.
        if OPERATOR_TERM.load(Ordering::SeqCst) && sigterm_at.is_none() {
            killed_by = Some("operator-sigterm");
            // SAFETY: as for the RSS kill above: plain integers, and `pid`
            // is our unreaped child.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            sigterm_at = Some(Instant::now());
        }

        // Grace expired and the worker still breathes — escalate.
        if !sigkill_sent {
            if let Some(at) = sigterm_at {
                if at.elapsed() > plan.kill_grace {
                    sigkill_sent = true;
                    // SAFETY: as for the RSS kill above: plain integers,
                    // and `pid` is our unreaped child.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }

        std::thread::sleep(POLL_INTERVAL);
    };

    // Prefer the kernel's peak-RSS over our sampled peak — the report
    // should carry the highest value observed by anyone, not just our poll
    // cadence. `ru_maxrss` is bytes on Darwin, KiB on Linux.
    #[cfg(target_os = "macos")]
    let kernel_peak = usage.ru_maxrss as u64;
    #[cfg(target_os = "linux")]
    let kernel_peak = (usage.ru_maxrss as u64).saturating_mul(1024);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let kernel_peak = 0u64;
    let peak_rss = peak_rss.max(kernel_peak);
    let cpu_ms = (usage.ru_utime.tv_sec as u64 * 1000 + usage.ru_utime.tv_usec as u64 / 1000)
        + (usage.ru_stime.tv_sec as u64 * 1000 + usage.ru_stime.tv_usec as u64 / 1000);
    let wall_ms = started.elapsed().as_millis() as u64;

    let (outcome, exit_code, effective_killed_by) = classify_death(status, killed_by);

    // The worker is gone, so its pipes are at EOF: collect what each
    // stream passed and dropped before the report — the report stays the
    // last line on stdout.
    let collect = |handle: Option<std::thread::JoinHandle<Forwarded>>| {
        handle.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    let stdout_fwd = collect(out_forwarder);
    let stderr_fwd = collect(err_forwarder);
    for (name, fwd) in [("stdout", stdout_fwd), ("stderr", stderr_fwd)] {
        if fwd.dropped > 0 {
            eprintln!(
                "\njsrt: supervise: worker {name}: {} bytes past the {cap}-byte cap were dropped \
                 (the last {} re-shown above)",
                fwd.dropped, fwd.tail
            );
        }
    }

    let report = json!({
        "run_id": run_id,
        "outcome": outcome.label,
        "exit_code": exit_code,
        "worker_pid": pid,
        "wall_ms": wall_ms,
        "cpu_ms": cpu_ms,
        "peak_rss_bytes": peak_rss,
        "limits": {
            "max_rss_bytes": plan.max_rss_bytes,
            "max_cpu_secs": plan.max_cpu_secs,
            "wall_ms": plan.wall.map(|d| d.as_millis() as u64),
        },
        "killed_by": effective_killed_by,
        "signal": outcome.signal,
        "poll_interval_ms": POLL_INTERVAL.as_millis(),
        "output": {
            "max_bytes_per_stream": cap,
            "rate_bytes_per_sec": plan.output_rate_bps,
            "redact_literals": plan.redact_literals.len(),
            "stdout_bytes": stdout_fwd.written,
            "stdout_dropped_bytes": stdout_fwd.dropped,
            "stdout_tail_bytes": stdout_fwd.tail,
            "stdout_redacted_bytes": stdout_fwd.redacted,
            "stderr_bytes": stderr_fwd.written,
            "stderr_dropped_bytes": stderr_fwd.dropped,
            "stderr_tail_bytes": stderr_fwd.tail,
            "stderr_redacted_bytes": stderr_fwd.redacted,
            "raw_passthrough": plan.output_raw,
            "tty_neutralised": {
                "stdout": stdout_tty && !plan.output_raw,
                "stderr": stderr_tty && !plan.output_raw,
            },
        },
    });

    emit(&report, plan.report_path.as_deref(), plan.report_max_bytes)?;
    Ok(exit_code)
}

struct Outcome {
    label: &'static str,
    signal: Option<i32>,
}

/// Map the child's wait-status onto the run taxonomy. A signal we sent is
/// a budget doing its job; a signal nobody asked for is a host failure —
/// the report names it either way. Returns the label, the exit code, and
/// the `killed_by` the report should carry (SIGXCPU explains itself; any
/// other signal names itself and admits the sender is unknown — claiming
/// a kill jsrt never ordered would be a lie).
fn classify_death(
    status: libc::c_int,
    killed_by: Option<&'static str>,
) -> (Outcome, i32, Option<String>) {
    if libc::WIFEXITED(status) {
        let code = libc::WEXITSTATUS(status);
        let label = match code {
            0 => "clean",
            2 => "usage",
            3 => "load",
            4 => "syntax",
            5 => "exception",
            6 => "terminated",
            7 => "capability",
            8 => "host",
            _ => "exited",
        };
        return (
            Outcome {
                label,
                signal: None,
            },
            code,
            killed_by.map(str::to_string),
        );
    }
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        // A SIGXCPU is the CPU rlimit, a SIGTERM/SIGKILL after sigterm_at
        // is our own hand, a SIGABRT is most likely V8's OOM handler
        // firing — all explained deaths. Anything else names the signal
        // and says the sender is unknown instead of implying jsrt sent it.
        let by = killed_by.map(str::to_string).or(if sig == libc::SIGXCPU {
            Some(KILLED_BY_CPU.to_string())
        } else if sig == libc::SIGABRT {
            Some("abort".to_string())
        } else {
            Some(format!("signal {sig} (sender unknown)"))
        });
        let expected = killed_by.is_some()
            || sig == libc::SIGXCPU
            || sig == libc::SIGABRT
            || sig == libc::SIGTERM
            || sig == libc::SIGKILL;
        let label = if expected { "terminated" } else { "host" };
        let code = if expected { 6 } else { 8 };
        return (
            Outcome {
                label,
                signal: Some(sig),
            },
            code,
            by,
        );
    }
    (
        Outcome {
            label: "host",
            signal: None,
        },
        8,
        killed_by.map(str::to_string),
    )
}

enum Reap {
    Exited(libc::c_int, libc::rusage),
    Running,
    Error(std::io::Error),
}

/// `wait4(WNOHANG)` — the only wait that hands back both the exit status
/// and the child's peak resource usage.
fn reap(pid: libc::pid_t) -> Reap {
    let mut status: libc::c_int = 0;
    // SAFETY: `rusage` is a C struct of integers and `timeval`s, for which
    // all-zero bytes are a valid value.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `status` and `usage` are live, writable locals of exactly the
    // types `wait4` writes, valid for the whole call.
    let ret = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut usage) };
    if ret == 0 {
        Reap::Running
    } else if ret < 0 {
        Reap::Error(std::io::Error::last_os_error())
    } else {
        Reap::Exited(status, usage)
    }
}

/// Current resident footprint. `proc_pid_rusage` is the honest source on
/// Darwin — the same number Activity Monitor shows, not a VM accounting.
#[cfg(target_os = "macos")]
fn resident_bytes(pid: libc::pid_t) -> Option<u64> {
    // SAFETY: `rusage_info_v4` is a C struct of integers, for which
    // all-zero bytes are a valid value.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live, writable `rusage_info_v4`, the struct that
    // RUSAGE_INFO_V4 asks the kernel to fill, so it writes within it.
    let ret = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            &mut info as *mut libc::rusage_info_v4 as *mut libc::rusage_info_t,
        )
    };
    if ret == 0 {
        Some(info.ri_phys_footprint)
    } else {
        None
    }
}

/// `/proc/<pid>/status` VmRSS on Linux — same role, different door.
#[cfg(target_os = "linux")]
fn resident_bytes(pid: libc::pid_t) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line
        .trim_start_matches("VmRSS:")
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb * 1024)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn resident_bytes(_pid: libc::pid_t) -> Option<u64> {
    None
}

fn emit(
    report: &serde_json::Value,
    path: Option<&std::path::Path>,
    max_bytes: u64,
) -> Result<(), HostError> {
    let line = serde_json::to_string(report)
        .map_err(|e| HostError::Host(format!("report serialize: {e}")))?;
    match path {
        Some(p) => append_bounded(p, &line, max_bytes),
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            writeln!(lock, "{line}").map_err(|e| HostError::Host(format!("report write: {e}")))
        }
    }
}

/// Append one JSONL record under a byte cap. When the next record would
/// overflow, the file rotates to `<name>.1` and the fresh file opens with
/// a meta-record saying how many records the rotation displaced — a
/// flooded log limits itself and *says so*, which is the whole point of
/// bounded diagnostics. One generation is kept; a single record larger
/// than the cap is written whole to a fresh file — boundedness never eats
/// a run's only evidence.
fn append_bounded(path: &std::path::Path, line: &str, max_bytes: u64) -> Result<(), HostError> {
    let current_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if current_len > 0 && current_len + line.len() as u64 + 1 > max_bytes {
        rotate(path).map_err(|e| HostError::Host(format!("report rotate: {e}")))?;
    }
    append_line(path, line)
}

/// Rename `path` to `<name>.1` and open a fresh file whose first record is
/// the rotation itself — carrying the count of records displaced.
fn rotate(path: &std::path::Path) -> std::io::Result<u64> {
    let dropped = count_records(path);
    let rotated = path.with_file_name(format!(
        "{}.1",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    std::fs::rename(path, &rotated)?;
    let meta = json!({
        "type": "rotation",
        "dropped_records": dropped,
        "rotated_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });
    let line = serde_json::to_string(&meta).map_err(|e| std::io::Error::other(e.to_string()))?;
    append_line(path, &line).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(dropped)
}

fn count_records(path: &std::path::Path) -> u64 {
    std::fs::read_to_string(path)
        .map(|c| c.lines().filter(|l| !l.trim().is_empty()).count() as u64)
        .unwrap_or(0)
}

fn append_line(path: &std::path::Path, line: &str) -> Result<(), HostError> {
    use std::fs::OpenOptions;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| HostError::Host(format!("report file {}: {e}", path.display())))?;
    writeln!(f, "{line}").map_err(|e| HostError::Host(format!("report file: {e}")))
}

#[cfg(test)]
mod tests {
    use super::{Neutraliser, Redactor};

    #[test]
    fn utf8_text_passes_the_neutraliser_byte_for_byte() {
        let mut n = Neutraliser::new();
        let mut out = Vec::new();
        n.process("café € 😀 日本語\n".as_bytes(), &mut out);
        n.finish(&mut out);
        assert_eq!(out, "café € 😀 日本語\n".as_bytes());
    }

    #[test]
    fn c1_code_points_render_but_lone_bytes_stay_hex() {
        let mut n = Neutraliser::new();
        let mut out = Vec::new();
        // U+009B (CSI) arrives on a UTF-8 terminal as C2 9B.
        n.process(&[0xc2, 0x9b], &mut out);
        assert_eq!(out, b"M-^[");
        out.clear();
        // The lone byte is the 8-bit form of the same control.
        n.process(&[0x9b], &mut out);
        assert_eq!(out, b"\\x9b");
    }

    #[test]
    fn a_scalar_split_across_chunks_arrives_intact() {
        let mut n = Neutraliser::new();
        let mut out = Vec::new();
        n.process(&[0xf0, 0x9f], &mut out); // first half of U+1F600
        assert!(out.is_empty(), "held: {out:?}");
        n.process(&[0x98, 0x80], &mut out);
        assert_eq!(out, &[0xf0, 0x9f, 0x98, 0x80]);
    }

    #[test]
    fn a_truncated_scalar_renders_at_eof() {
        let mut n = Neutraliser::new();
        let mut out = Vec::new();
        n.process(&[0xf0, 0x9f], &mut out);
        n.finish(&mut out);
        assert_eq!(out, b"\\xf0\\x9f");
    }

    #[test]
    fn invalid_and_control_bytes_stay_visible() {
        let mut n = Neutraliser::new();
        let mut out = Vec::new();
        n.process(
            &[
                0x1b, b'[', b'2', b'J', // CSI clear-screen attempt
                0x07, // BEL
                0xed, 0xa0, 0xbd, // unpaired surrogate (WTF-8)
                0xff, b'A', b'\n',
            ],
            &mut out,
        );
        assert_eq!(out, b"^[[2J^G\\xed\\xa0\\xbd\\xffA\n");
    }

    #[test]
    fn overlapping_secrets_mask_their_union() {
        let mut r = Redactor::new(vec![b"1234".to_vec(), b"345678".to_vec()]);
        let mut out = Vec::new();
        r.process(b"pre 12345678 post", &mut out);
        r.finish(&mut out);
        assert_eq!(out, b"pre [redacted] post");
        assert_eq!(r.redacted, 8);
    }
}

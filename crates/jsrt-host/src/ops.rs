//! The ops — every host capability a script can invoke.
//!
//! An op is the only door out of the isolate. If an ability isn't an op, the
//! script doesn't have it; if it is an op, it checks the capability manifest
//! first. Errors thrown here surface in JS with a `name` scripts can match —
//! `CapabilityError` for refusals, `TypeError`/`Error` for misuse.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use deno_core::{op2, OpState};
use deno_error::JsErrorBox;
use serde::Serialize;

use crate::capability::Capabilities;
use crate::error::HostError;

/// The default fetch deadline — a script may not hold a network op open
/// longer than this. `HostOptions::fetch_deadline` tightens it for drills.
pub const FETCH_DEADLINE: Duration = Duration::from_secs(10);
/// Largest response body `rt.fetch` will materialize.
pub const FETCH_MAX_BODY: u64 = 4 * 1024 * 1024;

/// Operation budgets a run carries in `OpState` — the knobs a host may tune
/// without touching the capability law itself.
#[derive(Debug, Clone)]
pub struct OpBudgets {
    pub fetch_deadline: Duration,
    pub fetch_max_body: u64,
}

impl Default for OpBudgets {
    fn default() -> Self {
        Self {
            fetch_deadline: FETCH_DEADLINE,
            fetch_max_body: FETCH_MAX_BODY,
        }
    }
}

/// Errors the host itself produced (I/O failures, deadlines, internal state)
/// surface as class `HostError` so an uncaught one classifies to exit 8 —
/// distinct from a script's own `Error` (exit 5).
fn host_err(msg: String) -> JsErrorBox {
    JsErrorBox::new("HostError", msg)
}

fn deny(err: HostError) -> JsErrorBox {
    match err {
        HostError::Capability(msg) => JsErrorBox::new("CapabilityError", msg),
        HostError::Host(msg) => host_err(msg),
        other => JsErrorBox::generic(other.to_string()),
    }
}

/// Pull the capability manifest out of `OpState` (stored as `Rc<Capabilities>`
/// at runtime build).
fn capabilities(state: &Rc<RefCell<OpState>>) -> Result<Rc<Capabilities>, JsErrorBox> {
    state
        .borrow()
        .try_borrow::<Rc<Capabilities>>()
        .cloned()
        .ok_or_else(|| host_err("capabilities not installed".into()))
}

fn http_client(state: &Rc<RefCell<OpState>>) -> Result<Rc<reqwest::Client>, JsErrorBox> {
    state
        .borrow()
        .try_borrow::<Rc<reqwest::Client>>()
        .cloned()
        .ok_or_else(|| host_err("http client not installed".into()))
}

fn budgets(state: &Rc<RefCell<OpState>>) -> OpBudgets {
    state
        .borrow()
        .try_borrow::<Rc<OpBudgets>>()
        .map(|b| (**b).clone())
        .unwrap_or_default()
}

/// `rt.hostInfo()` — who's hosting this script.
#[derive(Serialize)]
struct HostInfo {
    runtime: &'static str,
    version: &'static str,
    engine: &'static str,
}

#[op2]
#[serde]
fn op_host_info() -> Result<HostInfo, JsErrorBox> {
    Ok(HostInfo {
        runtime: "jsrt",
        version: env!("CARGO_PKG_VERSION"),
        engine: "v8",
    })
}

/// `rt.now()` — wall clock in milliseconds, for script-side measurements
/// (the perf harness times the op boundary with this).
#[op2(fast)]
fn op_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// `rt.sleep(ms)` — the async timer primitive the timer story builds on.
#[op2]
async fn op_sleep(ms: f64) -> Result<(), JsErrorBox> {
    if !ms.is_finite() || ms < 0.0 {
        return Err(JsErrorBox::type_error(
            "rt.sleep: ms must be a non-negative number",
        ));
    }
    tokio::time::sleep(Duration::from_secs_f64(ms / 1000.0)).await;
    Ok(())
}

/// `rt.readText(path)` — capability-gated file read. Throws `CapabilityError`
/// outside declared read paths.
#[op2]
#[string]
async fn op_read_text(
    state: Rc<RefCell<OpState>>,
    #[string] path: String,
) -> Result<String, JsErrorBox> {
    let caps = capabilities(&state)?;
    let resolved = caps.may_read(std::path::Path::new(&path)).map_err(deny)?;
    tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|e| host_err(format!("read {}: {e}", resolved.display())))
}

/// `rt.fetch(url)` — capability-gated, deadline'd, body-capped fetch.
#[derive(Serialize)]
struct FetchResult {
    status: u16,
    #[serde(rename = "statusText")]
    status_text: String,
    body: String,
    bytes: usize,
    #[serde(rename = "durationMs")]
    duration_ms: f64,
}

#[op2]
#[serde]
async fn op_fetch(
    state: Rc<RefCell<OpState>>,
    #[string] url: String,
) -> Result<FetchResult, JsErrorBox> {
    let caps = capabilities(&state)?;
    let client = http_client(&state)?;
    let budgets = budgets(&state);
    caps.may_net(&url).map_err(deny)?;
    let deadline = budgets.fetch_deadline;
    let max_body = budgets.fetch_max_body;
    let started = std::time::Instant::now();
    let mut response = tokio::time::timeout(deadline, client.get(&url).send())
        .await
        .map_err(|_| host_err(format!("fetch {url}: deadline {deadline:?} exceeded")))?
        .map_err(|e| host_err(format!("fetch {url}: {e}")))?;
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| host_err(format!("fetch {url}: body: {e}")))?
    {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > max_body {
            return Err(host_err(format!(
                "fetch {url}: body exceeded {max_body} bytes"
            )));
        }
    }
    Ok(FetchResult {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        body: String::from_utf8_lossy(&body).into_owned(),
        bytes: body.len(),
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

// ---- rt.kv — the embedded store ------------------------------------------
//
// One lazily-opened SQLite connection per run, held in OpState. Sync ops on
// purpose: the store is a local file, ops are bounded by SQLite speed, and
// an honest block beats a fake async. The grant lives in the state cell —
// the op asks the same question every other op asks: is this declared?

/// Per-run KV handle: the grant plus the connection opened on first use.
/// A script that never touches `rt.kv` never creates the file.
pub struct KvState {
    grant: crate::capability::KvGrant,
    conn: Option<rusqlite::Connection>,
}

impl KvState {
    pub fn new(grant: crate::capability::KvGrant) -> Self {
        Self { grant, conn: None }
    }

    fn conn(&mut self) -> Result<&rusqlite::Connection, JsErrorBox> {
        if self.conn.is_none() {
            let c = rusqlite::Connection::open(&self.grant.path)
                .map_err(|e| host_err(format!("kv open {}: {e}", self.grant.path.display())))?;
            c.execute_batch("CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);")
                .map_err(|e| host_err(format!("kv init: {e}")))?;
            self.conn = Some(c);
        }
        self.conn
            .as_ref()
            .ok_or_else(|| host_err("kv connection lost".into()))
    }
}

fn kv_cell(state: &Rc<RefCell<OpState>>) -> Result<Rc<RefCell<KvState>>, JsErrorBox> {
    state
        .borrow()
        .try_borrow::<Rc<RefCell<KvState>>>()
        .cloned()
        .ok_or_else(|| {
            JsErrorBox::new(
                "CapabilityError",
                "kv denied: no store declared in the manifest".to_string(),
            )
        })
}

/// `rt.kv.get(key)` — the value, or null. `serde_json::Value` because op2
/// doesn't take `Option<String>`; a null is the honest miss.
#[op2]
#[serde]
fn op_kv_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<serde_json::Value, JsErrorBox> {
    use rusqlite::OptionalExtension;
    let cell = kv_cell(&state)?;
    let mut kv = cell.borrow_mut();
    let conn = kv.conn()?;
    let hit: Option<String> = conn
        .query_row("SELECT v FROM kv WHERE k = ?1", [&key], |r| r.get(0))
        .optional()
        .map_err(|e| host_err(format!("kv get: {e}")))?;
    Ok(hit.map_or(serde_json::Value::Null, serde_json::Value::String))
}

/// `rt.kv.put(key, value)` — upsert under the file-size quota.
#[op2(fast)]
fn op_kv_put(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] value: String,
) -> Result<(), JsErrorBox> {
    let cell = kv_cell(&state)?;
    let mut kv = cell.borrow_mut();
    // The quota is the db file's own size — simple, honest, and what the
    // operator actually budgets. Projection is conservative: current file
    // bytes + payload + a page of SQLite slack.
    let file_bytes = std::fs::metadata(&kv.grant.path)
        .map(|m| m.len())
        .unwrap_or(0);
    let projected = file_bytes + key.len() as u64 + value.len() as u64 + 4096;
    if projected > kv.grant.max_bytes {
        return Err(JsErrorBox::new(
            "CapabilityError",
            format!(
                "kv quota exceeded: {} is {file_bytes}B; this write projects {projected}B over the {}B grant",
                kv.grant.path.display(),
                kv.grant.max_bytes
            ),
        ));
    }
    let conn = kv.conn()?;
    conn.execute(
        "INSERT INTO kv (k, v) VALUES (?1, ?2) \
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        [&key, &value],
    )
    .map_err(|e| host_err(format!("kv put: {e}")))?;
    Ok(())
}

/// `rt.kv.delete(key)` — whether anything was removed.
#[op2(fast)]
fn op_kv_delete(state: Rc<RefCell<OpState>>, #[string] key: String) -> Result<bool, JsErrorBox> {
    let cell = kv_cell(&state)?;
    let mut kv = cell.borrow_mut();
    let conn = kv.conn()?;
    let n = conn
        .execute("DELETE FROM kv WHERE k = ?1", [&key])
        .map_err(|e| host_err(format!("kv delete: {e}")))?;
    Ok(n > 0)
}

/// `rt.kv.list(prefix)` — `{key, value}` entries under an exact prefix
/// (substr match, not LIKE — a prefix is a prefix, not a pattern).
#[derive(Serialize)]
struct KvEntry {
    key: String,
    value: String,
}

#[op2]
#[serde]
fn op_kv_list(
    state: Rc<RefCell<OpState>>,
    #[string] prefix: String,
    limit: u32,
) -> Result<Vec<KvEntry>, JsErrorBox> {
    let cell = kv_cell(&state)?;
    let mut kv = cell.borrow_mut();
    let conn = kv.conn()?;
    let mut stmt = conn
        .prepare(
            "SELECT k, v FROM kv WHERE substr(k, 1, length(?1)) = ?1 \
             ORDER BY k LIMIT ?2",
        )
        .map_err(|e| host_err(format!("kv list: {e}")))?;
    let rows = stmt
        .query_map(rusqlite::params![prefix, limit], |r| {
            Ok(KvEntry {
                key: r.get(0)?,
                value: r.get(1)?,
            })
        })
        .map_err(|e| host_err(format!("kv list: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| host_err(format!("kv list row: {e}")))?);
    }
    Ok(out)
}

// ---- rt.media — streamed, content-addressed uploads -----------------------
//
// Local-disk backend: the script hands over a path it may read, the host
// streams it into the declared media root under its sha256 name. Two rules
// keep the story honest: the upload *streams* (64 KiB chunks, never a
// whole-file buffer) and a name is the content — dedup is free because the
// destination filename is the hash.

/// Chunk size for the upload stream — big enough to amortize syscalls,
/// small enough that "streaming" isn't a euphemism for buffering.
const MEDIA_CHUNK: usize = 64 * 1024;

fn media_grant(
    state: &Rc<RefCell<OpState>>,
) -> Result<Rc<crate::capability::MediaGrant>, JsErrorBox> {
    state
        .borrow()
        .try_borrow::<Rc<crate::capability::MediaGrant>>()
        .cloned()
        .ok_or_else(|| {
            JsErrorBox::new(
                "CapabilityError",
                "media denied: no store declared in the manifest".to_string(),
            )
        })
}

#[derive(Serialize)]
struct MediaPutResult {
    name: String,
    bytes: u64,
    sha256: String,
    /// "stored" is the only success state a local backend can report. A
    /// future network backend adds "uncertain" for mid-flight failures —
    /// never "uploaded" on less than the backend's own confirmation.
    state: &'static str,
}

/// `rt.media.put(srcPath)` — stream a read-granted file into the store.
/// Returns {name, bytes, sha256, state}; the name is the sha256 hex.
#[op2]
#[serde]
fn op_media_put(
    state: Rc<RefCell<OpState>>,
    #[string] src: String,
) -> Result<MediaPutResult, JsErrorBox> {
    use sha2::Digest;
    use std::io::{Read, Write};

    let caps = capabilities(&state)?;
    let grant = media_grant(&state)?;
    // Reading requires a read grant — media never bypasses that gate. The
    // check is is_read_allowed, NOT may_read: may_read's 4 MiB cap is a
    // materialization limit for readText, and this op never materializes.
    let resolved = std::fs::canonicalize(&src).map_err(|e| {
        JsErrorBox::new(
            "CapabilityError",
            format!("read denied: {src} does not resolve ({e})"),
        )
    })?;
    if !caps.is_read_allowed(&resolved) {
        return Err(JsErrorBox::new(
            "CapabilityError",
            format!(
                "read denied: {} is outside every declared read path",
                resolved.display()
            ),
        ));
    }
    let mut input = std::fs::File::open(&resolved)
        .map_err(|e| host_err(format!("media put: open {}: {e}", resolved.display())))?;

    // Unique-enough tmp name in the same dir as the destination — rename
    // across a filesystem boundary wouldn't be atomic.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = grant
        .root
        .join(format!(".media-tmp-{}-{nanos}", std::process::id()));

    let streamed = (|| -> Result<(String, u64), JsErrorBox> {
        let mut out = std::fs::File::create(&tmp)
            .map_err(|e| host_err(format!("media put: create tmp: {e}")))?;
        let mut hasher = sha2::Sha256::new();
        let mut total: u64 = 0;
        let mut buf = vec![0u8; MEDIA_CHUNK];
        loop {
            let n = input
                .read(&mut buf)
                .map_err(|e| host_err(format!("media put: read src: {e}")))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > grant.max_bytes {
                return Err(JsErrorBox::new(
                    "CapabilityError",
                    format!(
                        "media put: {} exceeds the {}-byte grant",
                        resolved.display(),
                        grant.max_bytes
                    ),
                ));
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| host_err(format!("media put: write tmp: {e}")))?;
        }
        out.sync_all()
            .map_err(|e| host_err(format!("media put: sync tmp: {e}")))?;
        Ok((format!("{:x}", hasher.finalize()), total))
    })();

    match streamed {
        Ok((hex, total)) => {
            let dest = grant.root.join(&hex);
            if dest.exists() {
                // The name is the content — already stored.
                let _ = std::fs::remove_file(&tmp);
            } else {
                std::fs::rename(&tmp, &dest)
                    .map_err(|e| host_err(format!("media put: commit: {e}")))?;
            }
            Ok(MediaPutResult {
                name: hex.clone(),
                bytes: total,
                sha256: hex,
                state: "stored",
            })
        }
        Err(e) => {
            // Whatever failed, the partial goes with us — the store only
            // ever contains complete, name-matches-content files.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// `rt.media.stat(name)` — {name, bytes} for a stored name, else null.
/// Names are validated, never joined blindly — a stat can't become a path.
#[op2]
#[serde]
fn op_media_stat(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let grant = media_grant(&state)?;
    let valid = name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if !valid {
        return Err(JsErrorBox::type_error(
            "rt.media.stat: name must be a 64-char lowercase sha256 hex",
        ));
    }
    let path = grant.root.join(&name);
    match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => Ok(serde_json::json!({ "name": name, "bytes": m.len() })),
        _ => Ok(serde_json::Value::Null),
    }
}

deno_core::extension!(
    jsrt_core,
    ops = [
        op_host_info,
        op_now,
        op_sleep,
        op_read_text,
        op_fetch,
        op_noop,
        op_kv_get,
        op_kv_put,
        op_kv_delete,
        op_kv_list,
        op_media_put,
        op_media_stat,
    ],
    esm_entry_point = "ext:jsrt_core/bootstrap.js",
    esm = [dir "js", "bootstrap.js"],
);

/// A no-op used by the perf harness to measure raw op-boundary overhead —
/// wired into the extension but absent from `bootstrap.js`, so scripts see it
/// only via `Deno.core.ops.op_noop` when explicitly told to.
#[op2(fast)]
fn op_noop() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_errors_keep_their_name() {
        use deno_error::JsErrorClass;
        let err = deny(HostError::Capability("read denied: /x".into()));
        assert_eq!(err.get_class(), "CapabilityError");
        assert_eq!(err.get_message().as_ref(), "read denied: /x");
    }
}

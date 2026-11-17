//! The capability-gated module loader.
//!
//! `FsModuleLoader` would happily resolve an `import` to any file on disk —
//! ops check `may_read`, but imports bypassed it entirely. This loader is the
//! same `FsModuleLoader` with the manifest's read paths enforced at both
//! doors: `resolve` (where the specifier is decided) and `load` (where the
//! bytes come through, and where the requested module type is known).
//!
//! Two fences, because an import is either code or data:
//! - a **JavaScript module** (no import attribute, a `.js`/`.mjs` file) may
//!   come from the entry's own directory — the operator chose that program,
//!   so its local module graph is program space — or from a declared read
//!   path;
//! - a **data import** (`with { type: "text" | "bytes" | "json" }`, or any
//!   other file imported as a module) is a file read like `rt.readText`:
//!   declared read paths only, under the same size cap. Without this,
//!   `jsrt run ~/x.js` would make every file under `~` importable as text.
//!
//! Modules come from local files only. Every specifier that is not
//! `file:` (`http:`, `https:`, `data:`, `blob:`, `node:`, `npm:`, a script
//! asking for `ext:`) is refused here, at `resolve` and again at `load`,
//! before anything is fetched or evaluated: a `net` grant is for
//! `rt.fetch`, not a source of code. Without this gate the only thing
//! stopping a remote import was `FsModuleLoader` failing to turn the URL
//! into a path, which is upstream behaviour, not a promise.
//!
//! The split that keeps the taxonomy honest:
//! - the **entry** module is the operator's choice — it resolves and loads
//!   unconditionally (a static load with no referrer is the root load);
//! - a **dependency** outside its fence is a capability refusal —
//!   `CapabilityError`, exit 7;
//! - a dependency that doesn't resolve at all is still a *load* failure (3)
//!   when it would be inside its fence — refusing to load a file that isn't
//!   there is the loader's job, not ours. Outside the fence it is refused
//!   like any other path there, before the filesystem is asked, so an
//!   import cannot tell a script which host files exist (see
//!   [`Capabilities::locate`]).

use std::path::Path;
use std::rc::Rc;

use deno_core::{
    FsModuleLoader, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader,
    ModuleResolveResponse, ModuleSpecifier, RequestedModuleType, ResolutionKind,
};
use deno_error::JsErrorBox;

use crate::capability::{Capabilities, Located};
use crate::error::HostError;

/// Host vocabulary the classifier recognizes: dynamic-import rejections lose
/// their error class at the V8 boundary (deno_core flattens them to
/// TypeError), so this prefix is how an uncaught refusal still classifies as
/// CapabilityError → exit 7. Only the loader emits it.
pub const IMPORT_DENIED_PREFIX: &str = "import denied:";

/// `FsModuleLoader` + the read-path manifest, enforced on every import.
pub struct JsrtModuleLoader {
    inner: FsModuleLoader,
    /// The manifest plus the entry's directory — where JS modules may live.
    program: Capabilities,
    /// The manifest alone — where data imports may come from.
    data: Capabilities,
}

impl JsrtModuleLoader {
    /// `entry_dir` as the operator gave it; it is kept canonicalized and as
    /// spelled, like a declared read root. One that does not resolve adds
    /// nothing.
    pub fn new(manifest: Capabilities, entry_dir: Option<&Path>) -> Self {
        let mut grants = manifest.clone().into_grants();
        if let Some(dir) = entry_dir {
            let _ = grants.add_read_root(dir);
        }
        Self {
            inner: FsModuleLoader,
            program: Capabilities::from_grants(grants),
            data: manifest,
        }
    }
}

fn denied(msg: String) -> JsErrorBox {
    JsErrorBox::new("CapabilityError", format!("{IMPORT_DENIED_PREFIX} {msg}"))
}

/// Where a `file:` specifier's path is, judged against `fence`. A file URL
/// carries an absolute path with `.` and `..` already folded. Other schemes
/// never get this far ([`local_only`] refuses them first); a `file:` URL
/// that names no local path is refused too.
fn located(fence: &Capabilities, specifier: &ModuleSpecifier) -> Located {
    match specifier.to_file_path() {
        Ok(path) => fence.locate(&path),
        Err(()) => Located::Refused,
    }
}

/// The one refusal for an import outside its fence — whether or not
/// anything is there.
fn outside(asked: &str) -> JsErrorBox {
    denied(format!("{asked} is outside every declared read path"))
}

/// A JavaScript module by both signals: no import attribute asked for
/// another type, and the real file (after symlinks) is `.js`/`.mjs`.
fn is_js_module(resolved: &Path, requested: &RequestedModuleType) -> bool {
    matches!(requested, RequestedModuleType::None)
        && resolved
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("js") || e.eq_ignore_ascii_case("mjs"))
}

/// Refuse any module source that is not a local file.
fn local_only(specifier: &ModuleSpecifier) -> Result<(), JsErrorBox> {
    if specifier.scheme() == "file" {
        return Ok(());
    }
    Err(denied(format!(
        "{}: imports are not supported; modules load from local files only",
        specifier.scheme()
    )))
}

impl JsrtModuleLoader {
    /// The resolve-time check: the type is not known yet, so only the wider
    /// program fence applies; `load` narrows it. A refusal names `asked`:
    /// the script can read the message (a caught dynamic import), so it
    /// gets back what it asked for, never the canonical path, which would
    /// show where the host's files live and where a symlink points.
    fn gate_resolve(&self, specifier: &ModuleSpecifier, asked: &str) -> Result<(), JsErrorBox> {
        local_only(specifier)?;
        match located(&self.program, specifier) {
            // Missing inside the fence: the inner loader's Load (3).
            Located::Found(_) | Located::Missing => Ok(()),
            Located::Refused => Err(outside(asked)),
        }
    }

    /// The load-time check, with the requested module type in hand.
    fn gate_load(
        &self,
        specifier: &ModuleSpecifier,
        requested: &RequestedModuleType,
    ) -> Result<(), JsErrorBox> {
        local_only(specifier)?;
        // Only the URL is known here, not what the script wrote: name the
        // file alone.
        let asked = specifier
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|name| !name.is_empty())
            .unwrap_or("the imported module")
            .to_string();
        let resolved = match located(&self.program, specifier) {
            Located::Found(resolved) => resolved,
            Located::Missing => return Ok(()),
            Located::Refused => return Err(outside(&asked)),
        };
        if is_js_module(&resolved, requested) {
            // The inner loader reads the whole file; a FIFO or a device
            // would block it, or stream without end.
            return if resolved.is_file() {
                Ok(())
            } else {
                Err(denied(format!("{asked} is not a regular file")))
            };
        }
        let refused = |msg: String| {
            denied(format!(
                "{msg} (the entry directory admits .js/.mjs modules only; text, bytes, \
                 JSON and other files need a declared read path)"
            ))
        };
        match located(&self.data, specifier) {
            Located::Found(resolved) => match self.data.read_cap(&resolved, &asked) {
                Ok(()) => Ok(()),
                Err(HostError::Capability(msg)) => Err(refused(msg)),
                Err(other) => Err(denied(other.to_string())),
            },
            // Inside the program fence, so it may know the file is there;
            // outside the data fence, so it may not read it.
            Located::Missing | Located::Refused => Err(refused(format!(
                "{asked} is outside every declared read path"
            ))),
        }
    }
}

impl ModuleLoader for JsrtModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> ModuleResolveResponse {
        let resolved = self.inner.resolve(specifier, referrer, kind)?;
        // Only a local file is ever a module, the entry included.
        local_only(&resolved)?;
        // Only the operator's entry is ungated; every import a *script*
        // makes (static or dynamic) must stay inside the fence.
        if !matches!(kind, ResolutionKind::MainModule) {
            self.gate_resolve(&resolved, specifier)?;
        }
        Ok(resolved)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        maybe_referrer: Option<&ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        // The root of a static load arrives with no referrer. So does the
        // root of a *dynamic* import (deno_core passes None there too), which
        // is why `is_dynamic_import` is part of the test: a script's
        // `import()` is never the entry.
        let is_entry = maybe_referrer.is_none() && !options.is_dynamic_import;
        // Checked again here, for any specifier that reached `load` without
        // going through `resolve` above.
        if let Err(e) = local_only(module_specifier) {
            return ModuleLoadResponse::Sync(Err(e));
        }
        if !is_entry {
            if let Err(e) = self.gate_load(module_specifier, &options.requested_module_type) {
                return ModuleLoadResponse::Sync(Err(e));
            }
        }
        self.inner.load(module_specifier, maybe_referrer, options)
    }
}

/// The loader handed to `JsRuntime` — the manifest and the entry directory
/// are baked in at build.
pub fn gated_loader(manifest: &Capabilities, entry_dir: Option<&Path>) -> Rc<dyn ModuleLoader> {
    Rc::new(JsrtModuleLoader::new(manifest.clone(), entry_dir))
}

//! The capability-gated module loader.
//!
//! `FsModuleLoader` would happily resolve an `import` to any file on disk —
//! ops check `may_read`, but imports bypassed it entirely. This loader is the
//! same `FsModuleLoader` with the manifest's read paths enforced at both
//! doors: `resolve` (where the specifier is decided) and `load` (where the
//! bytes come through).
//!
//! The split that keeps the taxonomy honest:
//! - the **entry** module is the operator's choice — it resolves and loads
//!   unconditionally (`maybe_referrer: None` marks the root load);
//! - a **dependency** that canonicalizes *outside* declared read paths is a
//!   capability refusal — `CapabilityError`, exit 7;
//! - a dependency that doesn't resolve at all is still a *load* failure (3) —
//!   refusing to load a file that isn't there is the loader's job, not ours.

use std::rc::Rc;

use deno_core::{
    FsModuleLoader, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader,
    ModuleResolveResponse, ModuleSpecifier, ResolutionKind,
};
use deno_error::JsErrorBox;

use crate::capability::Capabilities;

/// Host vocabulary the classifier recognizes: dynamic-import rejections lose
/// their error class at the V8 boundary (deno_core flattens them to
/// TypeError), so this prefix is how an uncaught refusal still classifies as
/// CapabilityError → exit 7. Only the loader emits it.
pub const IMPORT_DENIED_PREFIX: &str = "import denied:";

/// `FsModuleLoader` + the read-path manifest, enforced on every import.
pub struct JsrtModuleLoader {
    inner: FsModuleLoader,
    capabilities: Capabilities,
}

impl JsrtModuleLoader {
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            inner: FsModuleLoader,
            capabilities,
        }
    }
}

/// Refuse a file specifier that resolves outside declared read paths.
/// Non-file schemes and unresolvable paths pass through — the inner loader
/// produces their own (load-class) errors.
fn gate(specifier: &ModuleSpecifier, capabilities: &Capabilities) -> Result<(), JsErrorBox> {
    if specifier.scheme() != "file" {
        return Ok(());
    }
    let path = match specifier.to_file_path() {
        Ok(p) => p,
        Err(()) => return Ok(()),
    };
    // Missing/unresolvable: leave it to the loader — that's a Load (3),
    // not a refusal we invented.
    let resolved = match std::fs::canonicalize(&path) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    if capabilities.is_read_allowed(&resolved) {
        Ok(())
    } else {
        Err(JsErrorBox::new(
            "CapabilityError",
            format!(
                "{IMPORT_DENIED_PREFIX} {} is outside every declared read path",
                resolved.display()
            ),
        ))
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
        // Only the operator's entry is ungated; every import a *script*
        // makes (static or dynamic) must stay inside the fence.
        if !matches!(kind, ResolutionKind::MainModule) {
            gate(&resolved, &self.capabilities)?;
        }
        Ok(resolved)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        maybe_referrer: Option<&ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        // The root arrives with no referrer; dependencies always carry one.
        // Re-checking at load() closes the door a resolved specifier could
        // have been smuggled through.
        if maybe_referrer.is_some() {
            if let Err(e) = gate(module_specifier, &self.capabilities) {
                return ModuleLoadResponse::Sync(Err(e));
            }
        }
        self.inner.load(module_specifier, maybe_referrer, options)
    }
}

/// The loader handed to `JsRuntime` — the manifest is baked in at build.
pub fn gated_loader(capabilities: &Capabilities) -> Rc<dyn ModuleLoader> {
    Rc::new(JsrtModuleLoader::new(capabilities.clone()))
}

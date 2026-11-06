//! jsrt-host — the Rust host behind `jsrt`.
//!
//! V8 (via deno_core) executes JavaScript; this crate owns everything around
//! it: the capability manifest that gates every op, the error taxonomy that
//! classifies every way a run can end, and the watchdog that makes timeouts
//! real. The `rt.*` namespace scripts see is a thin shim over these ops —
//! the host is the contract.

pub mod capability;
pub mod error;
pub mod ops;
pub mod runtime;

pub use capability::Capabilities;
pub use error::HostError;
pub use runtime::{run_file, run_source, HostOptions};

//! The host's honest error taxonomy.
//!
//! Every way a run can end is a distinct variant so the CLI can exit with a
//! distinct code and callers never have to string-match. `Syntax` and
//! `Exception` are deliberately separate: a script that never compiled is not
//! a script that ran and threw.

use std::io;

/// How a run (or a host operation) ended.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The entry point could not be resolved or read — missing file, bad
    /// module specifier. The script never existed as far as V8 is concerned.
    #[error("load: {0}")]
    Load(String),

    /// The source was read but failed to compile (a V8 `SyntaxError`).
    #[error("syntax: {0}")]
    Syntax(String),

    /// The script ran and threw an uncaught error (or rejected its top-level
    /// promise).
    #[error("exception: {0}")]
    Exception(String),

    /// The isolate was terminated — the timeout watchdog fired, or V8 itself
    /// stopped execution. Not a script error: the script never got to finish.
    #[error("terminated: {0}")]
    Terminated(String),

    /// An op refused a capability the manifest did not grant. Surfaced to the
    /// script as a `CapabilityError`; surfaced to the operator as this variant
    /// when it escapes the script uncaught.
    #[error("capability: {0}")]
    Capability(String),

    /// The host itself failed — an I/O error reading the entry point, a
    /// broken manifest, an internal error. Never confused with script errors.
    #[error("host: {0}")]
    Host(String),
}

impl HostError {
    /// The stable exit code for this class of ending. The CLI contract:
    /// 0 ok · 2 usage · 3 load · 4 syntax · 5 exception · 6 terminated ·
    /// 7 capability · 8 host.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Load(_) => 3,
            Self::Syntax(_) => 4,
            Self::Exception(_) => 5,
            Self::Terminated(_) => 6,
            Self::Capability(_) => 7,
            Self::Host(_) => 8,
        }
    }
}

impl From<io::Error> for HostError {
    fn from(err: io::Error) -> Self {
        Self::Host(err.to_string())
    }
}

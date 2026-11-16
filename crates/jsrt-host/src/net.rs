//! The HTTP client behind `rt.fetch`, built so the checked destination is
//! the connected destination on every hop.
//!
//! `may_net` checks the URL a script asked for. Left at reqwest's defaults,
//! the client would then follow up to ten redirects to anywhere and honour
//! `HTTP(S)_PROXY` from the environment — either one connects somewhere the
//! manifest never named. So the client is built with:
//! - a redirect policy that re-runs `may_net` on every hop (and keeps the
//!   ten-hop bound), refusing with the destination it would have reached;
//! - no proxy: the connection goes to the host that was checked.

use std::fmt;

use crate::capability::Capabilities;

/// Most redirects one fetch follows — reqwest's own default bound, kept.
pub const MAX_REDIRECTS: usize = 10;

/// A refusal raised inside the client (a redirect hop outside the grant).
/// It travels through reqwest's error chain and comes back out as a
/// `CapabilityError` — see [`refusal`].
#[derive(Debug)]
pub struct NetDenied(pub String);

impl fmt::Display for NetDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NetDenied {}

/// Stored in `OpState` instead of a client when the builder failed (TLS
/// backend init), so `rt.fetch` reports why rather than "not installed".
pub struct ClientUnavailable(pub String);

/// The client every fetch in this run uses.
pub fn client(caps: &Capabilities) -> Result<reqwest::Client, reqwest::Error> {
    let caps = caps.clone();
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error(format!("more than {MAX_REDIRECTS} redirects"));
        }
        match caps.may_net(attempt.url().as_str()) {
            Ok(()) => attempt.follow(),
            Err(e) => attempt.error(NetDenied(format!("redirect refused: {e}"))),
        }
    });
    reqwest::Client::builder()
        .redirect(policy)
        .no_proxy()
        .build()
}

/// If this client error is one of our refusals, its message.
pub fn refusal(err: &reqwest::Error) -> Option<String> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if let Some(denied) = e.downcast_ref::<NetDenied>() {
            return Some(denied.0.clone());
        }
        source = e.source();
    }
    None
}

/// The error and its cause chain on one line — reqwest's own Display stops
/// at "error following redirect", which hides the reason.
pub fn describe(err: &reqwest::Error) -> String {
    let mut out = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(e) = source {
        let msg = e.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        source = e.source();
    }
    out
}

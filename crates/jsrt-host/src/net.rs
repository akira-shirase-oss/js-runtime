//! The HTTP client behind `rt.fetch`, built so the checked destination is
//! the connected destination on every hop.
//!
//! `may_net` checks the URL a script asked for. Left at reqwest's defaults,
//! the client would then follow up to ten redirects to anywhere and honour
//! `HTTP(S)_PROXY` from the environment — either one connects somewhere the
//! manifest never named. So the client is built with:
//! - a redirect policy that re-runs `may_net` on every hop (and keeps the
//!   ten-hop bound), refusing with the destination it would have reached;
//! - no proxy: the connection goes to the host that was checked;
//! - a resolver that admits only public addresses for a granted *name*.
//!   `may_net` matches the name's string; DNS decides where it connects,
//!   and a granted name that resolves (or is rebound) to loopback, a
//!   private range or a link-local metadata address would otherwise reach
//!   an internal service. The check runs on the resolver's own answer —
//!   the addresses it admits are the only ones the connector sees, so
//!   there is no window between check and connect. An IP-literal grant
//!   (`10.0.0.5:8080`, `127.0.0.1:3000`) is the operator naming the
//!   address itself and never goes through DNS.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

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
        .dns_resolver(Arc::new(PublicOnlyResolver))
        .build()
}

/// System DNS, filtered to public addresses — see the module docs.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            // Port 0: the connector sets the URL's port on what we return.
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let admitted = admit(&host, found)?;
            Ok(Box::new(admitted.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Keep the public addresses of a resolved name; refuse when none is left.
pub fn admit(host: &str, found: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, NetDenied> {
    let (public, private): (Vec<SocketAddr>, Vec<SocketAddr>) =
        found.into_iter().partition(|a| is_public(a.ip()));
    if public.is_empty() {
        let list: Vec<String> = private.iter().map(|a| a.ip().to_string()).collect();
        return Err(NetDenied(format!(
            "fetch denied: {host} resolves to [{}], not a public address — a name grant \
             reaches public hosts only; grant the address itself (ip:port) for a private one",
            list.join(", ")
        )));
    }
    Ok(public)
}

/// Whether an address is globally routable — not loopback, private,
/// link-local (cloud metadata lives at 169.254.169.254), shared CGNAT,
/// unspecified, multicast, broadcast, documentation, benchmarking or
/// reserved. IPv4 embedded in IPv6 (mapped, NAT64) is judged as IPv4.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let seg = v6.segments();
            if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [a, b] = seg[6].to_be_bytes();
                let [c, d] = seg[7].to_be_bytes();
                return is_public_v4(Ipv4Addr::new(a, b, c, d));
            }
            !(seg[..6] == [0, 0, 0, 0, 0, 0] // ::, ::1, IPv4-compatible
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || (seg[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)) // documentation
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0 // 0.0.0.0/8, "this network"
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || (a == 100 && (b & 0xc0) == 64) // 100.64.0.0/10 shared CGNAT
        || (a == 192 && b == 0 && c == 0) // 192.0.0.0/24 protocol assignments
        || ip.is_documentation()
        || (a == 198 && (b & 0xfe) == 18) // 198.18.0.0/15 benchmarking
        || ip.is_multicast()
        || a >= 240) // 240.0.0.0/4 reserved, incl. broadcast
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn internal_addresses_are_not_public() {
        for s in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
        ] {
            assert!(!is_public(ip(s)), "{s} must not count as public");
        }
    }

    #[test]
    fn routable_addresses_are_public() {
        for s in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(is_public(ip(s)), "{s} must count as public");
        }
    }

    #[test]
    fn admit_keeps_only_public_answers_and_refuses_an_all_private_one() {
        let public: SocketAddr = "8.8.8.8:0".parse().unwrap_or_else(|e| panic!("{e}"));
        let meta: SocketAddr = "169.254.169.254:0"
            .parse()
            .unwrap_or_else(|e| panic!("{e}"));
        let admitted = admit("mixed.example", vec![meta, public])
            .unwrap_or_else(|e| panic!("mixed answer must keep the public one: {e}"));
        assert_eq!(admitted, vec![public]);
        let refused = admit("rebound.example", vec![meta])
            .err()
            .unwrap_or_else(|| panic!("all-private answer must refuse"));
        assert!(refused.0.contains("169.254.169.254"), "{}", refused.0);
        assert!(refused.0.contains("rebound.example"), "{}", refused.0);
    }
}

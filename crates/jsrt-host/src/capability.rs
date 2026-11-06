//! The capability manifest — what a script is *allowed* to reach, enforced
//! inside every op rather than promised in a comment.
//!
//! Two rules keep this honest:
//! - **Deny by default.** No manifest line, no operation. A script with an
//!   empty manifest can compute and print, and nothing else.
//! - **Validate the real target.** Paths are canonicalized before comparison
//!   (symlinks and `..` cannot escape a declared directory), and URLs are
//!   checked by parsed host:port, never by string prefix.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::HostError;

/// Largest body `rt.readText` will return — a file larger than this is a
/// capability problem (declared reads are for configuration-sized text), not
/// a buffering accident.
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// What a script may reach. Every field is an allowlist; empty means denied.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// Canonicalized directories the script may read beneath.
    read_paths: Vec<PathBuf>,
    /// `host` or `host:port` destinations the script may fetch.
    net_hosts: Vec<String>,
}

/// The manifest file format (`jsrt.manifest.json`).
#[derive(Debug, Deserialize)]
struct Manifest {
    /// Directories (resolved relative to the manifest file) the script may
    /// read beneath. Symlinks are resolved before checking.
    #[serde(default)]
    read: Vec<PathBuf>,
    /// `host` or `host:port` entries. `host` alone allows any port.
    #[serde(default)]
    net: Vec<String>,
}

impl Capabilities {
    /// Nothing allowed — the correct starting point.
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// Build a capability set directly (tests, programmatic use).
    pub fn grant(read_paths: Vec<PathBuf>, net_hosts: Vec<String>) -> Self {
        Self {
            read_paths,
            net_hosts,
        }
    }

    /// Load a manifest file. Read entries are resolved relative to the
    /// manifest's own directory so a manifest describes *its* project, not
    /// the operator's cwd.
    pub fn from_manifest_file(path: &Path) -> Result<Self, HostError> {
        let raw = fs::read_to_string(path).map_err(|e| {
            HostError::Host(format!("cannot read manifest {}: {e}", path.display()))
        })?;
        let manifest: Manifest = serde_json::from_str(&raw)
            .map_err(|e| HostError::Host(format!("invalid manifest {}: {e}", path.display())))?;
        let base = path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let mut caps = Self::default();
        for dir in &manifest.read {
            let joined = base.join(dir);
            // Canonicalize at load time: the manifest line must point at a
            // real directory, and the stored entry is the *resolved* path.
            let resolved = fs::canonicalize(&joined).map_err(|e| {
                HostError::Host(format!(
                    "manifest read path {} does not resolve: {e}",
                    joined.display()
                ))
            })?;
            caps.read_paths.push(resolved);
        }
        caps.net_hosts = manifest.net;
        Ok(caps)
    }

    /// Check whether `path` may be read. Returns the canonical path on
    /// success — callers must use the returned path so the checked path is
    /// the read path (no TOCTOU window between check and use).
    pub fn may_read(&self, path: &Path) -> Result<PathBuf, HostError> {
        let resolved = fs::canonicalize(path).map_err(|e| {
            HostError::Capability(format!(
                "read denied: {} does not resolve ({e})",
                path.display()
            ))
        })?;
        let allowed = self
            .read_paths
            .iter()
            .any(|root| resolved.starts_with(root));
        if !allowed {
            return Err(HostError::Capability(format!(
                "read denied: {} is outside every declared read path",
                resolved.display()
            )));
        }
        if resolved.is_file() {
            let len = fs::metadata(&resolved).map_or(0, |m| m.len());
            if len > MAX_READ_BYTES {
                return Err(HostError::Capability(format!(
                    "read denied: {} is {len} bytes (limit {MAX_READ_BYTES})",
                    resolved.display()
                )));
            }
        }
        Ok(resolved)
    }

    /// Check whether a URL's authority is on the net allowlist. Entries of
    /// `host` allow any port; `host:port` requires an exact port match.
    pub fn may_net(&self, url: &str) -> Result<(), HostError> {
        let (host, port) = url_authority(url)
            .ok_or_else(|| HostError::Capability(format!("fetch denied: unparseable URL {url}")))?;
        let allowed = self.net_hosts.iter().any(|entry| {
            entry == host
                || (!entry.contains(':') && entry == host)
                || entry == &format!("{host}:{port}")
        });
        if !allowed {
            return Err(HostError::Capability(format!(
                "fetch denied: {host}:{port} is not a declared net destination"
            )));
        }
        Ok(())
    }

    /// True when no capabilities were granted — used by ops that want to give
    /// a clearer error than a bare refusal.
    pub fn is_empty(&self) -> bool {
        self.read_paths.is_empty() && self.net_hosts.is_empty()
    }

    /// Decompose into the raw grant lists (for callers that merge sources).
    pub fn into_grants(self) -> Grants {
        Grants {
            read_paths: self.read_paths,
            net_hosts: self.net_hosts,
        }
    }

    /// Rebuild from raw grant lists — the inverse of `into_grants`. Callers
    /// are responsible for having canonicalized paths already.
    pub fn from_grants(grants: Grants) -> Self {
        Self {
            read_paths: grants.read_paths,
            net_hosts: grants.net_hosts,
        }
    }
}

/// The raw grant lists, for merging a manifest with CLI flags.
#[derive(Debug, Default)]
pub struct Grants {
    pub read_paths: Vec<PathBuf>,
    pub net_hosts: Vec<String>,
}

/// Extract `host` and `port` (defaulted by scheme) from an http(s) URL
/// without a URL-parsing dependency — the check is deliberately narrow.
fn url_authority(url: &str) -> Option<(&str, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest.split('/').next()?;
    let authority = authority.split('@').next_back()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (authority, default_port),
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_parsing() {
        assert_eq!(url_authority("https://a.b/c"), Some(("a.b", 443)));
        assert_eq!(url_authority("http://a.b:8080/x"), Some(("a.b", 8080)));
        assert_eq!(url_authority("http://u:p@a.b/x"), Some(("a.b", 80)));
        assert_eq!(url_authority("file:///etc/passwd"), None);
    }

    #[test]
    fn deny_all_denies() {
        let caps = Capabilities::deny_all();
        assert!(caps.may_net("http://127.0.0.1:1/").is_err());
        assert!(caps.is_empty());
    }

    #[test]
    fn net_allowlist_matches_authority() {
        let caps = Capabilities::grant(vec![], vec!["127.0.0.1:9".into(), "example.com".into()]);
        assert!(caps.may_net("http://127.0.0.1:9/").is_ok());
        assert!(caps.may_net("http://127.0.0.1:10/").is_err());
        assert!(caps.may_net("https://example.com/a").is_ok());
        assert!(caps.may_net("https://other.example.com/a").is_err());
    }
}

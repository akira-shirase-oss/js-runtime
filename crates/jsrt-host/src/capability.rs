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

/// The default KV quota when the manifest doesn't declare one.
const DEFAULT_KV_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// The default per-upload media cap.
const DEFAULT_MEDIA_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// A declared embedded-KV store: the file the script may open and the
/// quota it must stay under. The `path`'s parent is canonicalized at
/// load time — the db file itself need not exist yet.
#[derive(Debug, Clone)]
pub struct KvGrant {
    pub path: PathBuf,
    pub max_bytes: u64,
}

/// A declared media store: the directory the script may write to and the
/// per-upload cap. The root is canonicalized at load time — it must exist.
#[derive(Debug, Clone)]
pub struct MediaGrant {
    pub root: PathBuf,
    pub max_bytes: u64,
}

/// What a script may reach. Every field is an allowlist; empty means denied.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// Canonicalized directories the script may read beneath.
    read_paths: Vec<PathBuf>,
    /// `host` or `host:port` destinations the script may fetch.
    net_hosts: Vec<String>,
    /// The embedded KV store, if declared.
    kv: Option<KvGrant>,
    /// The media store, if declared.
    media: Option<MediaGrant>,
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
    /// The KV grant: {"kv": {"path": "app.kv", "max_bytes": 67108864}}.
    #[serde(default)]
    kv: Option<ManifestKv>,
    /// The media grant: {"media": {"backend": "local", "root": "media/",
    /// "max_bytes": 268435456}}.
    #[serde(default)]
    media: Option<ManifestMedia>,
}

#[derive(Debug, Deserialize)]
struct ManifestKv {
    path: PathBuf,
    #[serde(default)]
    max_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ManifestMedia {
    /// Only "local" ships today — anything else is a manifest error, not a
    /// silent fallback.
    backend: String,
    root: PathBuf,
    #[serde(default)]
    max_bytes: Option<u64>,
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
            kv: None,
            media: None,
        }
    }

    /// Build with a KV grant attached (the CLI `--kv` path).
    pub fn grant_with_kv(
        read_paths: Vec<PathBuf>,
        net_hosts: Vec<String>,
        kv: Option<KvGrant>,
    ) -> Self {
        Self {
            read_paths,
            net_hosts,
            kv,
            media: None,
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
        if let Some(kv) = manifest.kv {
            let joined = base.join(&kv.path);
            // The db file itself may not exist yet — its parent must. The
            // stored path is canonicalized-parent + file name, same rule as
            // read paths.
            let parent = joined
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let canonical_parent = fs::canonicalize(&parent).map_err(|e| {
                HostError::Host(format!(
                    "manifest kv path {} parent does not resolve: {e}",
                    joined.display()
                ))
            })?;
            let file_name = joined
                .file_name()
                .map(|n| n.to_os_string())
                .ok_or_else(|| {
                    HostError::Host(format!(
                        "manifest kv path {} has no file name",
                        joined.display()
                    ))
                })?;
            caps.kv = Some(KvGrant {
                path: canonical_parent.join(file_name),
                max_bytes: kv.max_bytes.unwrap_or(DEFAULT_KV_MAX_BYTES),
            });
        }
        if let Some(media) = manifest.media {
            if media.backend != "local" {
                return Err(HostError::Host(format!(
                    "manifest media backend '{}' is not implemented — only 'local' ships today",
                    media.backend
                )));
            }
            let joined = base.join(&media.root);
            let root = fs::canonicalize(&joined).map_err(|e| {
                HostError::Host(format!(
                    "manifest media root {} does not resolve: {e}",
                    joined.display()
                ))
            })?;
            if !root.is_dir() {
                return Err(HostError::Host(format!(
                    "manifest media root {} is not a directory",
                    root.display()
                )));
            }
            caps.media = Some(MediaGrant {
                root,
                max_bytes: media.max_bytes.unwrap_or(DEFAULT_MEDIA_MAX_BYTES),
            });
        }
        Ok(caps)
    }

    /// The KV grant, if declared — cloned so the caller never holds a
    /// borrow into the manifest.
    pub fn may_kv(&self) -> Result<KvGrant, HostError> {
        self.kv.clone().ok_or_else(|| {
            HostError::Capability("kv denied: no store declared in the manifest".into())
        })
    }

    /// The media grant, if declared.
    pub fn may_media(&self) -> Result<MediaGrant, HostError> {
        self.media.clone().ok_or_else(|| {
            HostError::Capability("media denied: no store declared in the manifest".into())
        })
    }

    /// The allowlist check on an already-canonical path — the module
    /// loader's gate uses this so an unresolvable path stays a *load*
    /// failure instead of becoming a refusal it isn't.
    pub fn is_read_allowed(&self, resolved: &Path) -> bool {
        self.read_paths
            .iter()
            .any(|root| resolved.starts_with(root))
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
        if !self.is_read_allowed(&resolved) {
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
    /// Both sides are normalized (case, trailing DNS dot, IPv6 brackets)
    /// before comparison — the checked host is the one the client connects
    /// to, not the one that merely appears in the string.
    pub fn may_net(&self, url: &str) -> Result<(), HostError> {
        let (host, port) = url_authority(url)
            .ok_or_else(|| HostError::Capability(format!("fetch denied: unparseable URL {url}")))?;
        let allowed = self.net_hosts.iter().any(|entry| {
            match parse_net_grant(entry) {
                Some((gh, gp)) => gh == host && gp.is_none_or(|p| p == port),
                // An unparseable grant can never match — fail closed.
                None => false,
            }
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
        self.read_paths.is_empty()
            && self.net_hosts.is_empty()
            && self.kv.is_none()
            && self.media.is_none()
    }

    /// Decompose into the raw grant lists (for callers that merge sources).
    pub fn into_grants(self) -> Grants {
        Grants {
            read_paths: self.read_paths,
            net_hosts: self.net_hosts,
            kv: self.kv,
            media: self.media,
        }
    }

    /// Rebuild from raw grant lists — the inverse of `into_grants`. Callers
    /// are responsible for having canonicalized paths already.
    pub fn from_grants(grants: Grants) -> Self {
        Self {
            read_paths: grants.read_paths,
            net_hosts: grants.net_hosts,
            kv: grants.kv,
            media: grants.media,
        }
    }
}

/// The raw grant lists, for merging a manifest with CLI flags.
#[derive(Debug, Default)]
pub struct Grants {
    pub read_paths: Vec<PathBuf>,
    pub net_hosts: Vec<String>,
    pub kv: Option<KvGrant>,
    pub media: Option<MediaGrant>,
}

/// Extract `host` and `port` (defaulted by scheme) from an http(s) URL
/// without a URL-parsing dependency — the check is deliberately narrow.
/// The authority ends at the first of `/ \ ? #` — WHATWG parsers treat
/// `\` as a delimiter too, so `http://evil.com\@granted.com/` must not
/// read the host past the backslash. Userinfo is stripped before the
/// host:port split; the host is normalized (lowercase, trailing DNS dot,
/// IPv6 brackets) so the checked host is the connected host.
fn url_authority(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let default_port = if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    let authority = rest.split(['/', '\\', '?', '#']).next()?;
    // Userinfo: take the part after the LAST '@' — but only if what follows
    // still looks like an authority. 'a@b@c' keeps 'c'.
    let authority = authority.rsplit('@').next()?;
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        // Bracketed IPv6: host ends at ']', optional :port after.
        let (h, tail) = inner.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None if tail.is_empty() => default_port,
            _ => return None,
        };
        (h.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()?),
            None => (authority.to_string(), default_port),
        }
    };
    let host = host.to_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host).to_string();
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

/// Parse a grant entry into `(host, Option<port>)`. Accepts `host`,
/// `host:port`, `[v6]`, `[v6]:port`, or a bare multi-colon IPv6 host
/// (any port). Anything else never matches.
fn parse_net_grant(entry: &str) -> Option<(String, Option<u16>)> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    let (host, port) = if let Some(rest) = entry.strip_prefix('[') {
        let (h, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().ok()?),
            None if tail.is_empty() => None,
            _ => return None,
        };
        (h, port)
    } else if entry.matches(':').count() > 1 {
        // Bare IPv6 literal — no port possible.
        (entry, None)
    } else if let Some((h, p)) = entry.rsplit_once(':') {
        (h, Some(p.parse::<u16>().ok()?))
    } else {
        (entry, None)
    };
    let host = host.to_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host).to_string();
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(url: &str) -> Option<(String, u16)> {
        url_authority(url)
    }

    #[test]
    fn authority_parsing() {
        assert_eq!(auth("https://a.b/c"), Some(("a.b".into(), 443)));
        assert_eq!(auth("http://a.b:8080/x"), Some(("a.b".into(), 8080)));
        assert_eq!(auth("http://u:p@a.b/x"), Some(("a.b".into(), 80)));
        assert_eq!(auth("file:///etc/passwd"), None);
        assert_eq!(auth("ftp://a.b/x"), None);
    }

    #[test]
    fn authority_normalization() {
        // DNS is case-insensitive; the trailing dot is the same name.
        assert_eq!(auth("HTTP://A.B/x"), Some(("a.b".into(), 80)));
        assert_eq!(auth("http://a.b./x"), Some(("a.b".into(), 80)));
        assert_eq!(auth("http://[::1]:8080/x"), Some(("::1".into(), 8080)));
    }

    #[test]
    fn authority_escapes_are_refused_or_checked_correctly() {
        // The classic smuggle: WHATWG treats '\' as a delimiter, so the
        // connection really goes to evil.com — our parser must agree.
        assert_eq!(
            auth("http://evil.com\\@granted.com/x"),
            Some(("evil.com".into(), 80))
        );
        // '?' ends the authority; the '@granted.com' is in the query, not
        // the host.
        assert_eq!(
            auth("http://evil.com?@granted.com/"),
            Some(("evil.com".into(), 80))
        );
        assert_eq!(
            auth("http://evil.com#@granted.com/"),
            Some(("evil.com".into(), 80))
        );
        // Empty host / empty port / garbage ports fail closed.
        assert_eq!(auth("http://@/x"), None);
        assert_eq!(auth("http://a.b:/x"), None);
        assert_eq!(auth("http://a.b:99x/x"), None);
        // Userinfo can't hide the host: last '@' wins.
        assert_eq!(
            auth("http://granted.com@evil.com/x"),
            Some(("evil.com".into(), 80))
        );
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

    #[test]
    fn net_allowlist_uses_the_real_host() {
        let caps = Capabilities::grant(vec![], vec!["granted.com".into()]);
        // Every trick that would connect to another host must refuse.
        for url in [
            "http://evil.com\\@granted.com/x",
            "http://evil.com?@granted.com/",
            "http://granted.com.evil.com/x",
            "http://granted.com@evil.com/x",
        ] {
            assert!(caps.may_net(url).is_err(), "{url} must refuse");
        }
        // Equivalent spellings of the granted host still work.
        for url in [
            "http://granted.com/x",
            "http://GRANTED.COM./x",
            "http://userinfo@granted.com/x",
        ] {
            assert!(caps.may_net(url).is_ok(), "{url} must allow");
        }
    }

    #[test]
    fn net_grants_parse_ipv6() {
        let caps = Capabilities::grant(
            vec![],
            vec!["[::1]:8080".into(), "[fe80::1]".into(), "::2".into()],
        );
        assert!(caps.may_net("http://[::1]:8080/x").is_ok());
        assert!(caps.may_net("http://[::1]:9090/x").is_err());
        assert!(caps.may_net("http://[fe80::1]:1234/x").is_ok());
        assert!(caps.may_net("http://[::2]:7777/x").is_ok());
        assert!(caps.may_net("http://[::3]:7777/x").is_err());
    }

    #[test]
    fn net_grants_fail_closed_on_garbage() {
        let caps = Capabilities::grant(
            vec![],
            vec![
                "nonsense:::".into(),
                ":80".into(),
                "a.b:notaport".into(),
                "".into(),
            ],
        );
        assert!(caps.may_net("http://a.b/x").is_err());
        assert!(caps.may_net("http://nonsense/x").is_err());
    }
}

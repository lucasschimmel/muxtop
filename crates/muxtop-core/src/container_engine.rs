//! Container engine abstraction: `ContainerEngine` async trait + socket
//! auto-detection for Docker/Podman daemons.
//!
//! This module defines the interface only. The concrete `bollard`-based
//! implementation lives in `DockerEngine`.
//!
//! Refresh rate contract: the Collector is expected to invoke
//! [`ContainerEngine::list_and_stats`] at **0.5 Hz** (once every 2 s, see
//! ADR-05). Implementations should stay well under a 1 s budget at 100
//! containers.
//!
//! # Detection chain
//!
//! [`detect_socket`] tries, in order:
//! 1. `$DOCKER_HOST` environment variable (if parseable as `unix://…` or
//!    `tcp://…` / `http://…`)
//! 2. `/var/run/docker.sock`
//! 3. `$XDG_RUNTIME_DIR/podman/podman.sock` (Podman rootless)
//! 4. `/run/podman/podman.sock` (Podman system)
//!
//! Returns `None` if nothing is found. Callers treat this as
//! [`ContainersSnapshot::unavailable`](crate::containers::ContainersSnapshot::unavailable).
//!
//! Detection never performs I/O beyond `Path::exists` — reachability of a
//! detected socket is the concrete engine's job (E2).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;

use crate::containers::{ContainerSnapshot, EngineKind};

/// A resolved endpoint for the container engine API.
///
/// Not serialized — kept local to the server/client process that owns the
/// engine instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionTarget {
    /// Unix domain socket at this absolute path.
    Unix(PathBuf),
    /// HTTP(S) URL, e.g. `http://host:2375` or `https://host:2376`.
    Http(String),
}

/// Errors raised by a [`ContainerEngine`] implementation.
///
/// Bridges into [`CoreError`](crate::error::CoreError) via `#[from]`.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The underlying socket or TCP connection could not be established.
    #[error("container engine connection failed: {0}")]
    ConnectFailed(String),

    /// No container with the given id exists (raced with removal).
    #[error("container not found: {0}")]
    ContainerNotFound(String),

    /// The daemon refused the request (403 / EACCES on the socket).
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The daemon took longer than the allowed per-call budget.
    #[error("operation timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// Generic daemon-reported error carrying its message verbatim.
    #[error("engine error: {0}")]
    Other(String),
}

/// Abstraction over a running container engine (Docker or Podman).
///
/// Implementations MUST be safe to share across tokio tasks
/// (`Send + Sync + 'static`).
///
/// See ADR-01: async-trait is used to keep the trait object-safe; the
/// Collector holds `Option<Box<dyn ContainerEngine>>`.
#[async_trait]
pub trait ContainerEngine: Send + Sync {
    /// List every container (running + stopped) with populated CPU / memory /
    /// network / block-io stats.
    ///
    /// Implementations should filter out containers that vanish between the
    /// list call and the per-container stats call (`ContainerNotFound` during
    /// stats). Other failures bubble as [`EngineError`].
    async fn list_and_stats(&self) -> Result<Vec<ContainerSnapshot>, EngineError>;

    /// Graceful shutdown: sends `SIGTERM` and optionally waits `timeout_secs`
    /// before escalating (daemon-side behaviour).
    async fn stop(&self, id: &str, timeout_secs: Option<u64>) -> Result<(), EngineError>;

    /// Immediate `SIGKILL`.
    async fn kill(&self, id: &str) -> Result<(), EngineError>;

    /// Restart the container.
    async fn restart(&self, id: &str) -> Result<(), EngineError>;

    /// Attach `/bin/sh` with a daemon-side PTY, using the container's default user.
    /// Existing third-party engines can explicitly decline interactive exec.
    async fn exec_shell(
        &self,
        _id: &str,
        _size: crate::container_exec::ExecSize,
    ) -> Result<crate::container_exec::ExecSession, EngineError> {
        Err(EngineError::Other(
            "interactive exec is not supported by this engine".into(),
        ))
    }

    /// Reported engine kind (useful to drive the UI badge).
    fn kind(&self) -> EngineKind;
}

/// Env-variable lookup indirection (see ADR-03).
///
/// Tests provide a `FakeEnv`; production uses `StdEnv`.
pub trait EnvLookup {
    fn var(&self, name: &str) -> Option<String>;
}

/// Real environment backed by [`std::env::var`].
pub struct StdEnv;

impl EnvLookup for StdEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// Parse a `$DOCKER_HOST` value into a [`ConnectionTarget`].
///
/// Supported shapes:
/// * `unix:///absolute/path`
/// * `tcp://host:port`  → normalized to `http://host:port`
/// * `http://host:port`
/// * `https://host:port`
///
/// Returns `None` for malformed input — the caller falls through to the
/// filesystem candidate list.
pub fn parse_docker_host(raw: &str) -> Option<ConnectionTarget> {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("unix://") {
        if rest.is_empty() {
            return None;
        }
        return Some(ConnectionTarget::Unix(PathBuf::from(rest)));
    }
    if let Some(rest) = raw.strip_prefix("tcp://") {
        if rest.is_empty() {
            return None;
        }
        return Some(ConnectionTarget::Http(format!("http://{rest}")));
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Some(ConnectionTarget::Http(raw.to_string()));
    }
    None
}

/// Pure, injectable socket detection — the real [`detect_socket`] is a thin
/// wrapper over this.
///
/// Precedence:
/// 1. `env.var("DOCKER_HOST")` if it parses via [`parse_docker_host`].
/// 2. Otherwise, the first `candidates` entry where `Path::exists()` holds.
/// 3. Otherwise `None`.
///
/// Side effect: when the resolved target is a non-loopback HTTP endpoint
/// (MED-S4), emits a `tracing::warn!` once at detection time.
pub fn detect_with<E: EnvLookup>(env: &E, candidates: &[&Path]) -> Option<ConnectionTarget> {
    if let Some(raw) = env.var("DOCKER_HOST")
        && let Some(target) = parse_docker_host(&raw)
    {
        if let ConnectionTarget::Http(url) = &target
            && !http_host_is_loopback(url)
        {
            tracing::warn!(
                target: "muxtop::docker",
                host = %url,
                "DOCKER_HOST points to a non-loopback target — container metadata will be sent there",
            );
        }
        return Some(target);
    }
    for candidate in candidates {
        if candidate.exists() {
            return Some(ConnectionTarget::Unix(candidate.to_path_buf()));
        }
    }
    None
}

/// Extract the host portion of an `http(s)://host[:port][/path]` URL and
/// decide whether it parses to a loopback IP literal (or to the literal
/// hostname `localhost`).
///
/// Hand-rolled — adding the `url` crate just for this single check would pull
/// in 4 transitive deps for ~20 lines of work.
///
/// Conservative: when the host can't be parsed as an IP and isn't `localhost`,
/// we treat it as non-loopback (warn) rather than silently passing.
fn http_host_is_loopback(url: &str) -> bool {
    // Strip scheme.
    let after_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);

    // Drop optional userinfo (`user:pass@host`) — not standard for DOCKER_HOST
    // but doesn't hurt to be defensive.
    let after_userinfo = match after_scheme.rfind('@') {
        Some(idx) => &after_scheme[idx + 1..],
        None => after_scheme,
    };

    // Cut at first `/` or `?` — leaves `host[:port]`.
    let host_port = after_userinfo
        .split(['/', '?'])
        .next()
        .unwrap_or(after_userinfo);

    // Strip port. IPv6 literals are wrapped in `[…]` per RFC 3986.
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        // `[::1]:port` → take through `]`.
        match stripped.find(']') {
            Some(end) => &stripped[..end],
            None => stripped, // malformed; best-effort
        }
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };

    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    // Try parsing as an IP literal (v4 or v6). Non-IP hostnames cannot be
    // judged loopback without DNS — warn.
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// Resolve the production candidate list and call [`detect_with`] with the
/// real environment.
///
/// Order mirrors the module docs: `$DOCKER_HOST` > `/var/run/docker.sock` >
/// `$XDG_RUNTIME_DIR/podman/podman.sock` > `/run/podman/podman.sock`.
pub fn detect_socket() -> Option<ConnectionTarget> {
    let env = StdEnv;
    let podman_user: Option<PathBuf> = env
        .var("XDG_RUNTIME_DIR")
        .map(|x| PathBuf::from(x).join("podman/podman.sock"));

    let docker = Path::new("/var/run/docker.sock");
    let podman_system = Path::new("/run/podman/podman.sock");

    let mut candidates: Vec<&Path> = Vec::with_capacity(3);
    candidates.push(docker);
    if let Some(p) = podman_user.as_deref() {
        candidates.push(p);
    }
    candidates.push(podman_system);

    detect_with(&env, &candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs::File;
    use tempfile::tempdir;

    /// Test double for `EnvLookup` that returns pre-set values — zero global
    /// mutation, safe across parallel tests.
    #[derive(Default)]
    struct FakeEnv {
        vars: HashMap<String, String>,
    }

    impl FakeEnv {
        fn with(mut self, key: &str, value: &str) -> Self {
            self.vars.insert(key.into(), value.into());
            self
        }
    }

    impl EnvLookup for FakeEnv {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
    }

    // -------- parse_docker_host --------

    #[test]
    fn parse_unix_url() {
        assert_eq!(
            parse_docker_host("unix:///tmp/x"),
            Some(ConnectionTarget::Unix(PathBuf::from("/tmp/x")))
        );
    }

    #[test]
    fn parse_tcp_url_is_rewritten_to_http() {
        assert_eq!(
            parse_docker_host("tcp://h:2375"),
            Some(ConnectionTarget::Http("http://h:2375".into()))
        );
    }

    #[test]
    fn parse_http_url_passes_through() {
        assert_eq!(
            parse_docker_host("http://h:2375"),
            Some(ConnectionTarget::Http("http://h:2375".into()))
        );
    }

    #[test]
    fn parse_https_url_passes_through() {
        assert_eq!(
            parse_docker_host("https://h:2376"),
            Some(ConnectionTarget::Http("https://h:2376".into()))
        );
    }

    #[test]
    fn parse_rejects_empty_and_garbage() {
        assert_eq!(parse_docker_host("unix://"), None);
        assert_eq!(parse_docker_host("tcp://"), None);
        assert_eq!(parse_docker_host("garbage"), None);
        assert_eq!(parse_docker_host(""), None);
    }

    // -------- detect_with --------

    #[test]
    fn detect_with_docker_host_unix_url() {
        let env = FakeEnv::default().with("DOCKER_HOST", "unix:///tmp/x");
        assert_eq!(
            detect_with(&env, &[]),
            Some(ConnectionTarget::Unix(PathBuf::from("/tmp/x")))
        );
    }

    #[test]
    fn detect_with_docker_host_tcp_url() {
        let env = FakeEnv::default().with("DOCKER_HOST", "tcp://h:2375");
        assert_eq!(
            detect_with(&env, &[]),
            Some(ConnectionTarget::Http("http://h:2375".into()))
        );
    }

    #[test]
    fn detect_with_fallback_picks_first_existing() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.sock");
        let second = dir.path().join("second.sock");
        File::create(&first).unwrap();
        File::create(&second).unwrap();

        let env = FakeEnv::default();
        let result = detect_with(&env, &[&first, &second]);
        assert_eq!(result, Some(ConnectionTarget::Unix(first)));
    }

    #[test]
    fn detect_with_fallback_skips_missing() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing.sock");
        let present = dir.path().join("present.sock");
        File::create(&present).unwrap();

        let env = FakeEnv::default();
        let result = detect_with(&env, &[&missing, &present]);
        assert_eq!(result, Some(ConnectionTarget::Unix(present)));
    }

    #[test]
    fn detect_with_returns_none_when_nothing_found() {
        let env = FakeEnv::default();
        assert_eq!(detect_with(&env, &[]), None);
    }

    #[test]
    fn detect_with_malformed_docker_host_falls_through_to_filesystem() {
        let dir = tempdir().unwrap();
        let present = dir.path().join("present.sock");
        File::create(&present).unwrap();

        let env = FakeEnv::default().with("DOCKER_HOST", "not-a-valid-url");
        let result = detect_with(&env, &[&present]);
        assert_eq!(result, Some(ConnectionTarget::Unix(present)));
    }

    #[test]
    fn detect_with_empty_docker_host_falls_through() {
        let dir = tempdir().unwrap();
        let present = dir.path().join("present.sock");
        File::create(&present).unwrap();

        let env = FakeEnv::default().with("DOCKER_HOST", "");
        let result = detect_with(&env, &[&present]);
        assert_eq!(result, Some(ConnectionTarget::Unix(present)));
    }

    // -------- EngineError --------

    #[test]
    fn engine_error_display_is_informative() {
        let variants: Vec<EngineError> = vec![
            EngineError::ConnectFailed("connection refused".into()),
            EngineError::ContainerNotFound("abc123".into()),
            EngineError::PermissionDenied("docker group".into()),
            EngineError::Timeout(std::time::Duration::from_secs(3)),
            EngineError::Other("daemon panic".into()),
        ];
        for err in &variants {
            let msg = format!("{err}");
            assert!(!msg.is_empty(), "empty Display for {err:?}");
        }
        // Spot-check that contextual strings are surfaced, not swallowed.
        assert!(format!("{}", variants[0]).contains("connection refused"));
        assert!(format!("{}", variants[1]).contains("abc123"));
    }

    #[test]
    fn engine_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EngineError>();
    }

    // -------- http_host_is_loopback (MED-S4) --------

    #[test]
    fn loopback_ipv4_is_loopback() {
        assert!(http_host_is_loopback("http://127.0.0.1:2375"));
        assert!(http_host_is_loopback("http://127.1.2.3"));
        assert!(http_host_is_loopback("https://127.0.0.1:2376/v1.40/info"));
    }

    #[test]
    fn loopback_ipv6_is_loopback() {
        assert!(http_host_is_loopback("http://[::1]:2375"));
        assert!(http_host_is_loopback("http://[::1]"));
    }

    #[test]
    fn localhost_literal_is_loopback() {
        assert!(http_host_is_loopback("http://localhost:2375"));
        assert!(http_host_is_loopback("https://LOCALHOST"));
    }

    #[test]
    fn rfc1918_addresses_are_not_loopback() {
        assert!(!http_host_is_loopback("http://10.0.0.1:2375"));
        assert!(!http_host_is_loopback("http://192.168.1.10:2375"));
        assert!(!http_host_is_loopback("https://172.16.0.1"));
    }

    #[test]
    fn public_ipv4_is_not_loopback() {
        assert!(!http_host_is_loopback("http://1.2.3.4:2375"));
        assert!(!http_host_is_loopback("https://203.0.113.5"));
    }

    #[test]
    fn arbitrary_hostname_is_not_loopback() {
        // Conservative: can't be sure without DNS, so we warn.
        assert!(!http_host_is_loopback("http://docker.example.com:2375"));
    }

    #[test]
    fn detect_with_non_loopback_docker_host_returns_target() {
        // Smoke test: warning is fired (not asserted here — we don't wire a
        // tracing subscriber), and the returned target is unchanged.
        let env = FakeEnv::default().with("DOCKER_HOST", "tcp://10.0.0.1:2375");
        assert_eq!(
            detect_with(&env, &[]),
            Some(ConnectionTarget::Http("http://10.0.0.1:2375".into()))
        );
    }

    // -------- integration: real detect_socket() is callable --------

    #[test]
    fn detect_socket_does_not_panic() {
        // Just ensure calling the real wrapper is sound; it may return None or
        // Some(...) depending on the host and we don't want to flake CI on
        // that.
        let _ = detect_socket();
    }
}

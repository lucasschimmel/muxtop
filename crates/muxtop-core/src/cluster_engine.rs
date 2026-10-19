//! Cluster engine abstraction: `ClusterEngine` async trait + kubeconfig
//! source detection for Kubernetes clusters.
//!
//! This module defines the interface only. The concrete `kube-rs`-based
//! implementation lives in `KubeEngine`.
//!
//! Refresh rate contract: the Collector is expected to invoke
//! [`ClusterEngine::snapshot`] at **0.2 Hz** (once every 5 s, see ADR-05).
//! Independent resource and metrics polling loops maintain caches;
//! `snapshot()` reads from those in-memory stores without network I/O.
//!
//! # Detection chain
//!
//! [`detect_kubeconfig`] tries, in order:
//! 1. `$KUBECONFIG` environment variable (path or platform-separated list —
//!    the engine splits and merges it; detection keeps the raw value).
//! 2. `~/.kube/config` (typical user kubeconfig).
//! 3. In-cluster ServiceAccount: existence of
//!    `/var/run/secrets/kubernetes.io/serviceaccount/token`.
//!
//! Returns [`KubeconfigSource::None`] if nothing is found. Callers treat this
//! as a `KubeSnapshot` with `reachable = false`.
//!
//! Detection never performs I/O beyond `Path::exists` and reading the env —
//! reachability of the cluster is the concrete engine's job (E2).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;

use crate::container_engine::EnvLookup;
use crate::kube::{ClusterKind, KubeSnapshot};

/// Errors raised by a [`ClusterEngine`] implementation.
///
/// Bridges into [`CoreError`](crate::error::CoreError) via `#[from]`.
///
/// Granularity matches the v0.3 `EngineError` (cf. ADR-01) but adds two
/// Kubernetes-specific variants: per-resource RBAC ([`Self::Forbidden`]) and
/// metrics-server absence ([`Self::MetricsUnavailable`]). [`Self::Stale`]
/// captures the case where the polling cache has not received a successful
/// resource list within the freshness contract.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClusterError {
    /// No kubeconfig source could be resolved (no `$KUBECONFIG`, no
    /// `~/.kube/config`, no in-cluster ServiceAccount).
    #[error("no kubeconfig found")]
    KubeconfigNotFound,

    /// The Kubernetes API server could not be reached (DNS, TCP, TLS, or
    /// `/version` probe failed).
    #[error("cluster unreachable: {0}")]
    Unreachable(String),

    /// RBAC denied a list/watch on a specific resource. `namespace` is
    /// `None` for cluster-scoped resources (Nodes) or all-namespaces
    /// queries.
    #[error("forbidden: cannot access {resource}{}", match namespace {
        Some(ns) => format!(" in namespace {ns}"),
        None => String::new(),
    })]
    Forbidden {
        resource: &'static str,
        namespace: Option<String>,
    },

    /// `metrics.k8s.io/v1beta1` is not served by this cluster (metrics-server
    /// missing or disabled). Renders CPU/MEM columns as `—` in the UI.
    #[error("metrics-server unavailable on this cluster")]
    MetricsUnavailable,

    /// The cached snapshot is older than the freshness contract — the
    /// polling task has not completed a successful list in time.
    #[error("snapshot stale (last update {since_secs}s ago)")]
    Stale { since_secs: u64 },

    /// Generic engine-reported error carrying its message verbatim.
    #[error("cluster engine error: {0}")]
    Other(String),
}

/// A resolved source for the Kubernetes client configuration.
///
/// Not serialized — kept local to the server/client process that owns the
/// engine instance. **The kubeconfig content itself never crosses the wire**
/// (cf. v0.4 plan, T-824 / T-854 anti-leak guards).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KubeconfigSource {
    /// Path resolved from `$KUBECONFIG`. May be a single file or a
    /// platform-separated list — the engine splits and merges the files.
    Env(PathBuf),
    /// Default user kubeconfig at `~/.kube/config`.
    Home(PathBuf),
    /// In-cluster service-account credentials at
    /// `/var/run/secrets/kubernetes.io/serviceaccount/`.
    InCluster,
    /// No kubeconfig found anywhere.
    None,
}

/// Which namespaces a [`ClusterEngine`] lists namespaced resources from.
///
/// Pods and Deployments honour this. **Nodes do not** — `Node` is a
/// cluster-scoped resource in Kubernetes with no namespaced variant, so it
/// is always listed cluster-wide and simply degrades to an empty list when
/// RBAC denies it (see [`ClusterError::Forbidden`]).
///
/// [`Self::AllNamespaces`] needs cluster-scoped `list` on the resource;
/// [`Self::Namespace`] works with a Role bound to that one namespace, which
/// is the common case for a developer on a shared cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum KubeScope {
    /// List across every namespace (`Api::all`).
    #[default]
    AllNamespaces,
    /// List a single namespace (`Api::namespaced`).
    Namespace(String),
}

impl KubeScope {
    /// The namespace this scope targets, or `None` when cluster-wide.
    pub fn namespace(&self) -> Option<&str> {
        match self {
            Self::AllNamespaces => None,
            Self::Namespace(ns) => Some(ns.as_str()),
        }
    }

    /// Value for [`KubeSnapshot::current_namespace`]: the namespace name when
    /// scoped, the empty string when cluster-wide.
    ///
    /// The empty string is the wire-level encoding of "all namespaces" — it
    /// avoids adding a field to [`KubeSnapshot`], which would be a breaking
    /// bincode change for a purely presentational distinction.
    pub fn label(&self) -> &str {
        self.namespace().unwrap_or("")
    }

    /// `true` when listing cluster-wide.
    pub fn is_all_namespaces(&self) -> bool {
        matches!(self, Self::AllNamespaces)
    }
}

/// Validate a Kubernetes namespace name against the DNS-1123 label rules the
/// API server enforces: 1–63 characters, lowercase alphanumeric or `-`,
/// starting and ending alphanumeric.
///
/// Beyond rejecting names the API server would refuse anyway, this is a
/// **security boundary**: the metrics-server poll interpolates the namespace
/// into a request path (`/apis/metrics.k8s.io/v1beta1/namespaces/{ns}/pods`),
/// and the namespace originates from `--kube-namespace` — user input. The
/// character set admitted here contains no `/`, `.`, `?` or `%`, so a
/// traversal or query-injection payload cannot reach the URI builder.
pub fn is_valid_namespace(name: &str) -> bool {
    if name.is_empty() || name.len() > 63 {
        return false;
    }
    let bytes = name.as_bytes();
    let is_alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !is_alnum(bytes[0]) || !is_alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes.iter().all(|&b| is_alnum(b) || b == b'-')
}

/// [`is_valid_namespace`] as a parser, for CLI argument validation.
///
/// Lives here rather than in the binaries so `muxtop` and `muxtop-server`
/// reject the same inputs with the same wording. Takes no clap dependency —
/// the signature is what clap's `value_parser` accepts, nothing more.
pub fn parse_namespace(raw: &str) -> Result<String, String> {
    if is_valid_namespace(raw) {
        Ok(raw.to_string())
    } else {
        Err(format!(
            "invalid namespace {raw:?}: expected a DNS-1123 label \
             (1-63 chars, lowercase alphanumeric or '-', \
             starting and ending alphanumeric)"
        ))
    }
}

/// Abstraction over a running Kubernetes cluster (Docker Desktop, kind,
/// k3s, EKS, GKE, AKS, OpenShift, plain kubeadm, …).
///
/// Implementations MUST be safe to share across tokio tasks
/// (`Send + Sync + 'static`).
///
/// See ADR-04 (`kube-rs vs k8s-openapi direct`): the production implementation
/// `KubeEngine` wraps `kube::Client` and polling caches for Pod / Node /
/// Deployment, plus a 5 s polling loop against
/// `metrics.k8s.io/v1beta1`. `snapshot()` reads from the in-memory stores
/// and is therefore CPU-only — the network cost is borne by the poll loops.
///
/// See ADR-01 (v0.3): `#[async_trait]` is used to keep the trait object-safe;
/// the Collector holds `Option<Arc<dyn ClusterEngine + Send + Sync>>`.
#[async_trait]
pub trait ClusterEngine: Send + Sync {
    /// Build a fresh snapshot of the cluster's pods, nodes and deployments
    /// from the in-memory reflector caches.
    ///
    /// Implementations MUST NOT block on network I/O here — use the watcher
    /// stream + metrics polling tasks for that. This call is on the hot
    /// path of the collector tick and budgeted to < 50 ms even with 1000+
    /// pods (cf. v0.4 plan T-816).
    async fn snapshot(&self) -> Result<KubeSnapshot, ClusterError>;

    /// Probe the API discovery for `metrics.k8s.io/v1beta1`. Result is cached
    /// by the implementation (typically 60 s) since this drives a UI badge.
    async fn metrics_available(&self) -> bool;

    /// Reported cluster fingerprint, used to label the UI.
    fn kind(&self) -> ClusterKind;

    /// `serverVersion.gitVersion` from the discovery API. `None` until the
    /// initial probe succeeds.
    fn server_version(&self) -> Option<&str>;

    /// The namespace scope currently applied to namespaced resources.
    ///
    /// Defaults to [`KubeScope::AllNamespaces`] so implementations without
    /// runtime rescoping need not override it.
    async fn scope(&self) -> KubeScope {
        KubeScope::AllNamespaces
    }

    /// Flip between cluster-wide listing and the engine's configured
    /// namespace, returning the scope now in effect.
    ///
    /// Bound to `A` in the Kube tab. The engine owns the flip rather than the
    /// caller because only the engine knows which namespace to scope *to*
    /// (`--kube-namespace` when given, otherwise the kubeconfig context's
    /// default namespace) — a caller holding only a snapshot cannot tell.
    ///
    /// Defaults to a no-op reporting [`KubeScope::AllNamespaces`].
    async fn toggle_scope(&self) -> KubeScope {
        KubeScope::AllNamespaces
    }
}

/// Pure, injectable kubeconfig detection — the real [`detect_kubeconfig`] is
/// a thin wrapper over this.
///
/// Precedence:
/// 1. `env.var("KUBECONFIG")` if non-empty.
/// 2. `home_kubeconfig` if `Some(p)` and `p.exists()`.
/// 3. `in_cluster_token` if it exists on disk.
/// 4. Otherwise [`KubeconfigSource::None`].
pub fn detect_kubeconfig_with<E: EnvLookup>(
    env: &E,
    home_kubeconfig: Option<&Path>,
    in_cluster_token: &Path,
) -> KubeconfigSource {
    if let Some(raw) = env.var("KUBECONFIG") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return KubeconfigSource::Env(PathBuf::from(trimmed));
        }
    }
    if let Some(home) = home_kubeconfig
        && home.exists()
    {
        return KubeconfigSource::Home(home.to_path_buf());
    }
    if in_cluster_token.exists() {
        return KubeconfigSource::InCluster;
    }
    KubeconfigSource::None
}

/// Resolve the production kubeconfig source using the real environment and
/// canonical paths.
///
/// * `home_kubeconfig` = `dirs::home_dir().map(|h| h.join(".kube/config"))`
/// * `in_cluster_token` = `/var/run/secrets/kubernetes.io/serviceaccount/token`
pub fn detect_kubeconfig() -> KubeconfigSource {
    use crate::container_engine::StdEnv;

    let env = StdEnv;
    let home_kubeconfig: Option<PathBuf> = dirs::home_dir().map(|h| h.join(".kube/config"));
    let in_cluster_token = Path::new("/var/run/secrets/kubernetes.io/serviceaccount/token");

    detect_kubeconfig_with(&env, home_kubeconfig.as_deref(), in_cluster_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs::File;
    use tempfile::tempdir;

    /// Test double for `EnvLookup`. Identical idiom to
    /// `container_engine::tests::FakeEnv` — duplicated locally to keep test
    /// modules independent (the upstream one is not `pub`).
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

    // -------- detect_kubeconfig_with --------

    #[test]
    fn detect_with_env_var_returns_env() {
        let env = FakeEnv::default().with("KUBECONFIG", "/etc/k8s/admin.conf");
        let dir = tempdir().unwrap();
        let token = dir.path().join("nope-token");

        let result = detect_kubeconfig_with(&env, None, &token);
        assert_eq!(
            result,
            KubeconfigSource::Env(PathBuf::from("/etc/k8s/admin.conf"))
        );
    }

    #[test]
    fn detect_with_env_var_supports_colon_separated_list() {
        // We keep the raw string; kube-rs handles the colon-split.
        let env = FakeEnv::default().with("KUBECONFIG", "/a/config:/b/config");
        let dir = tempdir().unwrap();
        let token = dir.path().join("nope-token");

        let result = detect_kubeconfig_with(&env, None, &token);
        assert_eq!(
            result,
            KubeconfigSource::Env(PathBuf::from("/a/config:/b/config"))
        );
    }

    #[test]
    fn detect_with_empty_env_var_falls_through_to_home() {
        let env = FakeEnv::default().with("KUBECONFIG", "");
        let dir = tempdir().unwrap();
        let home_config = dir.path().join("config");
        File::create(&home_config).unwrap();
        let token = dir.path().join("nope-token");

        let result = detect_kubeconfig_with(&env, Some(&home_config), &token);
        assert_eq!(result, KubeconfigSource::Home(home_config));
    }

    #[test]
    fn detect_with_whitespace_only_env_var_falls_through() {
        let env = FakeEnv::default().with("KUBECONFIG", "   ");
        let dir = tempdir().unwrap();
        let token = dir.path().join("nope-token");

        let result = detect_kubeconfig_with(&env, None, &token);
        assert_eq!(result, KubeconfigSource::None);
    }

    #[test]
    fn detect_with_home_kubeconfig_returns_home() {
        let env = FakeEnv::default();
        let dir = tempdir().unwrap();
        let home_config = dir.path().join("config");
        File::create(&home_config).unwrap();
        let token = dir.path().join("nope-token");

        let result = detect_kubeconfig_with(&env, Some(&home_config), &token);
        assert_eq!(result, KubeconfigSource::Home(home_config));
    }

    #[test]
    fn detect_with_home_kubeconfig_missing_falls_through() {
        let env = FakeEnv::default();
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let token = dir.path().join("nope-token");

        let result = detect_kubeconfig_with(&env, Some(&missing), &token);
        assert_eq!(result, KubeconfigSource::None);
    }

    #[test]
    fn detect_with_in_cluster_token_returns_in_cluster() {
        let env = FakeEnv::default();
        let dir = tempdir().unwrap();
        let token = dir.path().join("token");
        File::create(&token).unwrap();

        let result = detect_kubeconfig_with(&env, None, &token);
        assert_eq!(result, KubeconfigSource::InCluster);
    }

    #[test]
    fn detect_with_returns_none_when_nothing_present() {
        let env = FakeEnv::default();
        let dir = tempdir().unwrap();
        let nope_home = dir.path().join("nope-home");
        let nope_token = dir.path().join("nope-token");

        let result = detect_kubeconfig_with(&env, Some(&nope_home), &nope_token);
        assert_eq!(result, KubeconfigSource::None);
    }

    #[test]
    fn detect_priority_env_beats_home_and_in_cluster() {
        // All three sources active — env wins.
        let env = FakeEnv::default().with("KUBECONFIG", "/explicit/config");
        let dir = tempdir().unwrap();
        let home = dir.path().join("config");
        File::create(&home).unwrap();
        let token = dir.path().join("token");
        File::create(&token).unwrap();

        let result = detect_kubeconfig_with(&env, Some(&home), &token);
        assert_eq!(
            result,
            KubeconfigSource::Env(PathBuf::from("/explicit/config"))
        );
    }

    #[test]
    fn detect_priority_home_beats_in_cluster() {
        // Env empty, home and in-cluster both present — home wins.
        let env = FakeEnv::default();
        let dir = tempdir().unwrap();
        let home = dir.path().join("config");
        File::create(&home).unwrap();
        let token = dir.path().join("token");
        File::create(&token).unwrap();

        let result = detect_kubeconfig_with(&env, Some(&home), &token);
        assert_eq!(result, KubeconfigSource::Home(home));
    }

    // -------- integration: real detect_kubeconfig() is callable --------

    #[test]
    fn detect_kubeconfig_does_not_panic() {
        // Just ensure the production wrapper is sound; result depends on the
        // host so we don't pin it.
        let _ = detect_kubeconfig();
    }

    // -------- KubeScope --------

    #[test]
    fn scope_all_namespaces_has_no_namespace() {
        let scope = KubeScope::AllNamespaces;
        assert_eq!(scope.namespace(), None);
        assert_eq!(scope.label(), "");
        assert!(scope.is_all_namespaces());
    }

    #[test]
    fn scope_namespace_exposes_it() {
        let scope = KubeScope::Namespace("kube-system".into());
        assert_eq!(scope.namespace(), Some("kube-system"));
        assert_eq!(scope.label(), "kube-system");
        assert!(!scope.is_all_namespaces());
    }

    #[test]
    fn scope_defaults_to_all_namespaces() {
        // The default must stay cluster-wide: it is what an engine built
        // without `--kube-namespace` uses, and narrowing it would silently
        // hide resources for every existing user.
        assert_eq!(KubeScope::default(), KubeScope::AllNamespaces);
    }

    // -------- namespace validation --------

    #[test]
    fn valid_namespaces_are_accepted() {
        for ns in [
            "default",
            "kube-system",
            "a",
            "1",
            "my-app-2",
            &"a".repeat(63),
        ] {
            assert!(is_valid_namespace(ns), "expected {ns:?} to be valid");
        }
    }

    #[test]
    fn invalid_namespaces_are_rejected() {
        for ns in [
            "",               // empty
            &"a".repeat(64),  // one over the DNS-1123 limit
            "Default",        // uppercase
            "-leading",       // leading dash
            "trailing-",      // trailing dash
            "has_underscore", // underscore
            "has.dot",        // dot
            "has space",      // space
            "ns/../other",    // path traversal
            "ns/pods",        // extra path segment
            "ns?watch=true",  // query injection
            "ns%2f",          // percent-encoded slash
            "ns\u{0000}",     // NUL
            "ns\u{001b}[31m", // ANSI escape
        ] {
            assert!(!is_valid_namespace(ns), "expected {ns:?} to be rejected");
        }
    }

    #[test]
    fn namespace_rejection_blocks_uri_injection() {
        // The metrics poll interpolates the namespace into
        // `/apis/metrics.k8s.io/v1beta1/namespaces/{ns}/pods`. Every
        // character the validator admits must be URI-path-safe, so no
        // accepted namespace can add a segment or a query string.
        for ns in ["default", "kube-system", "my-app-2"] {
            assert!(is_valid_namespace(ns));
            assert!(!ns.contains(['/', '?', '#', '%', '.', ':', '@']));
        }
    }

    #[test]
    fn parse_namespace_round_trips_valid_input() {
        assert_eq!(parse_namespace("kube-system").unwrap(), "kube-system");
    }

    #[test]
    fn parse_namespace_reports_why_it_failed() {
        let err = parse_namespace("Bad NS").unwrap_err();
        assert!(err.contains("DNS-1123"), "unhelpful message: {err}");
        assert!(err.contains("Bad NS"), "message omits the input: {err}");
    }

    // -------- ClusterError --------

    #[test]
    fn cluster_error_display_is_informative() {
        let variants: Vec<ClusterError> = vec![
            ClusterError::KubeconfigNotFound,
            ClusterError::Unreachable("dns failed".into()),
            ClusterError::Forbidden {
                resource: "pods",
                namespace: Some("kube-system".into()),
            },
            ClusterError::Forbidden {
                resource: "nodes",
                namespace: None,
            },
            ClusterError::MetricsUnavailable,
            ClusterError::Stale { since_secs: 42 },
            ClusterError::Other("kaboom".into()),
        ];
        for err in &variants {
            let msg = format!("{err}");
            assert!(!msg.is_empty(), "empty Display for {err:?}");
        }
        // Spot-check that contextual content is surfaced.
        assert!(format!("{}", variants[1]).contains("dns failed"));
        let scoped = format!("{}", variants[2]);
        assert!(scoped.contains("pods"));
        assert!(scoped.contains("kube-system"));
        let cluster_scoped = format!("{}", variants[3]);
        assert!(cluster_scoped.contains("nodes"));
        assert!(!cluster_scoped.contains("namespace"));
        assert!(format!("{}", variants[5]).contains("42"));
    }

    #[test]
    fn cluster_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ClusterError>();
    }

    // -------- ClusterEngine trait shape --------

    /// Trivially-correct stub used only to assert the trait shape compiles
    /// in a `Box<dyn ClusterEngine>` (object-safety regression guard).
    /// The real impl lives in `kube_engine.rs` (E2).
    struct StubCluster;

    #[async_trait::async_trait]
    impl ClusterEngine for StubCluster {
        async fn snapshot(&self) -> Result<KubeSnapshot, ClusterError> {
            Ok(KubeSnapshot::unavailable())
        }

        async fn metrics_available(&self) -> bool {
            false
        }

        fn kind(&self) -> ClusterKind {
            ClusterKind::Generic
        }

        fn server_version(&self) -> Option<&str> {
            None
        }
    }

    #[test]
    fn cluster_engine_is_object_safe() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn ClusterEngine>();

        // Build through the dyn pointer — proves dyn-safety end-to-end.
        let _boxed: Box<dyn ClusterEngine + Send + Sync> = Box::new(StubCluster);
    }

    #[tokio::test]
    async fn cluster_engine_scope_defaults_are_cluster_wide() {
        // `StubCluster` overrides neither method — the defaults must be
        // usable, so adding scope support did not break third-party impls.
        let stub: Box<dyn ClusterEngine + Send + Sync> = Box::new(StubCluster);
        assert_eq!(stub.scope().await, KubeScope::AllNamespaces);
        assert_eq!(stub.toggle_scope().await, KubeScope::AllNamespaces);
    }

    #[tokio::test]
    async fn cluster_engine_stub_returns_unavailable() {
        let stub: Box<dyn ClusterEngine + Send + Sync> = Box::new(StubCluster);
        let snap = stub.snapshot().await.expect("stub never errors");
        assert!(!snap.reachable);
        assert!(snap.pods.is_empty());
        assert_eq!(stub.kind(), ClusterKind::Generic);
        assert!(stub.server_version().is_none());
        assert!(!stub.metrics_available().await);
    }
}

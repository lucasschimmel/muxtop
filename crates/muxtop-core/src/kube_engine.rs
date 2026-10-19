//! Concrete `kube-rs`-backed implementation of [`ClusterEngine`].
//!
//! # Architecture
//!
//! See ADR-04 (`kube-rs vs k8s-openapi direct`, accepted 2026-04-26) and
//! ADR-05 (poll vs reflectors, accepted 2026-04-26 — see
//! `.claude/output/forge/32-v04-kubernetes-epics/`).
//!
//! v0.4 ships a **poll-based** design rather than the reflector-based design
//! initially scoped: a single tokio task spawned from [`KubeEngine::connect`]
//! wakes every 5 s, calls `Api::<K>::list()` for Pods / Nodes / Deployments,
//! and writes the raw objects into a shared `ResourceCache`. A second task
//! polls `metrics.k8s.io/v1beta1` on the same cadence, filling
//! `MetricsCache`. [`ClusterEngine::snapshot`] is therefore CPU-only —
//! it reads both caches and runs the typed-to-snapshot conversion.
//!
//! Reflectors / `kube::runtime::watcher` were considered but deferred to a
//! follow-up (ADR-05): the watcher API in 0.99 has a heavier mock surface
//! that would have doubled the test code, and the poll cadence (5 s) is
//! identical to what the user-facing UI tick uses anyway. If a perf
//! measurement at v0.4.x scale (>1000 pods) shows the LIST traffic is
//! material, switching is mechanically straightforward — it's an internal
//! detail of `KubeEngine`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::ListParams;
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cluster_engine::{
    ClusterEngine, ClusterError, KubeScope, KubeconfigSource, is_valid_namespace,
};
use crate::kube::{
    ClusterKind, DeploymentSnapshot, DeploymentStrategy, KubeSnapshot, NodeSnapshot, NodeStatus,
    PodPhase, PodSnapshot, QosClass,
};

// ---- Caches --------------------------------------------------------------

/// Raw API objects produced by the 5 s poll loop. Conversion to the wire
/// snapshot types runs in `snapshot()` so we never block on I/O there.
#[derive(Default)]
pub(crate) struct ResourceCache {
    pub pods: Vec<Pod>,
    pub nodes: Vec<Node>,
    pub deployments: Vec<Deployment>,
    /// Last successful list, in Unix milliseconds. Production expiry uses
    /// the monotonic `last_success` clock to tolerate system clock changes.
    pub last_update_ms: u64,
    poll_state: Option<ResourcePollState>,
    last_success: Option<tokio::time::Instant>,
}

/// Retains the distinction between a fully healthy API, restricted/partial
/// access and total failure without changing the serialized snapshot schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResourcePollState {
    Complete,
    Partial,
    Failed,
}

#[derive(Clone)]
struct ScopeState {
    scope: KubeScope,
    generation: u64,
}

impl ScopeState {
    fn new(scope: KubeScope) -> Self {
        Self {
            scope,
            generation: 0,
        }
    }
}

/// Metrics-server cache — populated by the metrics polling task.
///
/// `pods`/`nodes` are looked up from the snapshot conversion path; entries
/// missing from the map render as `cpu_millis = None` / `mem_bytes = None`
/// in the wire snapshot, which the UI surfaces as `—`.
#[derive(Default)]
pub(crate) struct MetricsCache {
    /// Whether `/apis/metrics.k8s.io/v1beta1` answered the last probe.
    pub available: bool,
    /// `(namespace, pod_name) -> (cpu_millis, mem_bytes)`.
    pub pods: HashMap<(String, String), (u32, u64)>,
    /// `node_name -> (cpu_millis, mem_bytes)`.
    pub nodes: HashMap<String, (u32, u64)>,
}

// ---- Engine --------------------------------------------------------------

/// `kube-rs`-backed [`ClusterEngine`].
///
/// Construction goes through [`KubeEngine::connect`] for production paths
/// or `KubeEngine::new_for_test` for unit tests
/// that prepopulate the caches by hand.
pub struct KubeEngine {
    cluster_kind: ClusterKind,
    server_version: Option<String>,
    /// Namespace used whenever the engine is scoped: `--kube-namespace` when
    /// the user passed one, otherwise the active kubeconfig context's default
    /// namespace. This is what [`ClusterEngine::toggle_scope`] scopes *to*
    /// when the engine is currently cluster-wide.
    scoped_namespace: String,
    /// Live scope shared with both poll loops — [`ClusterEngine::toggle_scope`]
    /// writes it and the next tick reads it.
    scope: Arc<RwLock<ScopeState>>,
    resources: Arc<RwLock<ResourceCache>>,
    metrics: Arc<RwLock<MetricsCache>>,
    /// Cancels the spawned poll task on drop.
    cancel: CancellationToken,
}

/// Poll cadence for both the resource list task and the metrics-server
/// task. Matches the [`ClusterEngine::snapshot`] tick rate the collector
/// uses (5 s — see ADR-05).
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Per-list timeout — guards the snapshot freshness contract by bounding
/// how long a single `Api::list` can hold up the loop.
const LIST_TIMEOUT: Duration = Duration::from_secs(3);

impl KubeEngine {
    /// Production path — builds a `kube::Client` from `source`, probes
    /// `/version` to fingerprint the cluster, and spawns the resource +
    /// metrics poll tasks. The returned engine starts with empty caches;
    /// the first useful [`ClusterEngine::snapshot`] arrives ~5 s later
    /// once the poll loop has run.
    pub async fn connect(
        source: KubeconfigSource,
        context: Option<&str>,
        namespace: Option<&str>,
    ) -> Result<Self, ClusterError> {
        // Reject a malformed namespace before it can reach the metrics-server
        // URI builder (see `is_valid_namespace`). The CLI validates too, but
        // this is the library boundary and must not rely on its callers.
        if let Some(ns) = namespace
            && !is_valid_namespace(ns)
        {
            return Err(ClusterError::Other(format!(
                "invalid namespace {ns:?}: expected a DNS-1123 label \
                 (1-63 chars, lowercase alphanumeric or '-', \
                 starting and ending alphanumeric)"
            )));
        }

        let config = build_config(source, context, namespace).await?;
        let resolved_namespace = config.default_namespace.clone();

        let client = Client::try_from(config)
            .map_err(|e| ClusterError::Unreachable(format!("client init failed: {e}")))?;

        // Probe /version to fingerprint the cluster. We don't fail the
        // connection on probe failure — the cluster may still be usable
        // for list calls; the badge just falls back to `Generic`.
        let (cluster_kind, server_version) = match probe_version(&client).await {
            Ok((kind, version)) => (kind, Some(version)),
            Err(_) => (ClusterKind::Generic, None),
        };

        let resources = Arc::new(RwLock::new(ResourceCache::default()));
        let metrics = Arc::new(RwLock::new(MetricsCache::default()));
        let cancel = CancellationToken::new();

        // `--kube-namespace` scopes the engine; without it we keep the v0.4
        // behaviour of listing cluster-wide, so upgrading changes nothing for
        // users who never passed the flag.
        let initial_scope = match namespace {
            Some(ns) => KubeScope::Namespace(ns.to_string()),
            None => KubeScope::AllNamespaces,
        };
        // Toggling from cluster-wide needs a namespace to land on: the flag
        // when given, else whatever the kubeconfig context declares.
        let scoped_namespace = namespace
            .map(String::from)
            .unwrap_or_else(|| resolved_namespace.clone());
        let scope = Arc::new(RwLock::new(ScopeState::new(initial_scope)));

        // Resource poll task.
        let _resource_handle = spawn_resource_loop(
            client.clone(),
            resources.clone(),
            scope.clone(),
            cancel.clone(),
        );
        // Metrics poll task.
        let _metrics_handle = spawn_metrics_loop(
            client.clone(),
            metrics.clone(),
            scope.clone(),
            cancel.clone(),
        );

        Ok(Self {
            cluster_kind,
            server_version,
            scoped_namespace,
            scope,
            resources,
            metrics,
            cancel,
        })
    }

    /// Set the namespace scope directly. Inherent rather than part of
    /// [`ClusterEngine`] because the trait only needs the toggle; tests and
    /// future callers that know exactly which scope they want use this.
    ///
    /// Clears the pod and deployment caches so a scope-down never leaves
    /// out-of-scope rows on screen until the next poll tick.
    pub async fn set_scope(&self, scope: KubeScope) {
        // Hold the scope write lock through cache invalidation. Publishers
        // hold its read lock through publication, closing the check/write race.
        let mut current = self.scope.write().await;
        current.scope = scope;
        current.generation = current.generation.wrapping_add(1);
        let mut cache = self.resources.write().await;
        cache.pods.clear();
        cache.deployments.clear();
        cache.last_update_ms = 0;
        cache.last_success = None;
        cache.poll_state = None;
        *self.metrics.write().await = MetricsCache::default();
    }

    /// Test constructor that bypasses the network entirely. The caches are
    /// expected to be filled with hand-crafted `Pod` / `Node` / `Deployment`
    /// objects (typically via `serde_json::from_value`) and metrics rows.
    ///
    /// `pub(crate)` because [`ResourceCache`] / [`MetricsCache`] are
    /// implementation details — never exposed to consumers of muxtop-core.
    #[doc(hidden)]
    #[cfg(test)]
    /// `current_namespace` is interpreted the same way the wire field is: an
    /// empty string means cluster-wide, anything else scopes to that
    /// namespace.
    pub(crate) fn new_for_test(
        cluster_kind: ClusterKind,
        server_version: Option<String>,
        current_namespace: String,
        resources: ResourceCache,
        metrics: MetricsCache,
    ) -> Self {
        let scope = if current_namespace.is_empty() {
            KubeScope::AllNamespaces
        } else {
            KubeScope::Namespace(current_namespace.clone())
        };
        let scoped_namespace = if current_namespace.is_empty() {
            "default".to_string()
        } else {
            current_namespace
        };
        Self {
            cluster_kind,
            server_version,
            scoped_namespace,
            scope: Arc::new(RwLock::new(ScopeState::new(scope))),
            resources: Arc::new(RwLock::new(resources)),
            metrics: Arc::new(RwLock::new(metrics)),
            cancel: CancellationToken::new(),
        }
    }
}

/// Bench-only entry point: convert a flat list of typed Pods into a
/// `KubeSnapshot` using the same conversion path that `KubeEngine::snapshot`
/// uses internally. Exposed `pub` (with `#[doc(hidden)]`) so the
/// `kube_bench` Criterion benchmark can drive the conversion without
/// reaching into `pub(crate)` internals.
///
/// Not intended for production callers — the regular path is to attach a
/// `KubeEngine` to the [`crate::collector::Collector`].
#[doc(hidden)]
pub fn build_kube_snapshot_for_bench(
    pods: Vec<k8s_openapi::api::core::v1::Pod>,
    pod_metrics: std::collections::HashMap<(String, String), (u32, u64)>,
) -> KubeSnapshot {
    let now_ms = unix_ms();
    let metrics = MetricsCache {
        available: !pod_metrics.is_empty(),
        pods: pod_metrics,
        nodes: std::collections::HashMap::new(),
    };
    let pod_snaps: Vec<PodSnapshot> = pods
        .iter()
        .map(|p| pod_to_snapshot(p, &metrics, now_ms))
        .collect();
    KubeSnapshot {
        cluster_kind: ClusterKind::Generic,
        server_version: None,
        current_namespace: String::new(),
        reachable: true,
        metrics_available: metrics.available,
        pods: pod_snaps,
        nodes: Vec::new(),
        deployments: Vec::new(),
    }
}

impl Drop for KubeEngine {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[async_trait]
impl ClusterEngine for KubeEngine {
    async fn snapshot(&self) -> Result<KubeSnapshot, ClusterError> {
        let scope = self.scope.read().await;
        let resources = self.resources.read().await;
        let metrics = self.metrics.read().await;

        let now_ms = unix_ms();
        let age = resources
            .last_success
            .map(|last| last.elapsed())
            .unwrap_or_else(|| {
                Duration::from_millis(now_ms.saturating_sub(resources.last_update_ms))
            });
        if resources.last_update_ms > 0 && age > POLL_INTERVAL * 3 {
            return Err(ClusterError::Stale {
                since_secs: age.as_secs(),
            });
        }
        let pods: Vec<PodSnapshot> = resources
            .pods
            .iter()
            .map(|p| pod_to_snapshot(p, &metrics, now_ms))
            .collect();
        let mut nodes: Vec<NodeSnapshot> = resources
            .nodes
            .iter()
            .map(|n| node_to_snapshot(n, &metrics, now_ms))
            .collect();

        // Cross-resource join: a single `Node` object carries no pod count,
        // so the aggregation belongs here, where both lists are in hand.
        let scheduled = pods_per_node(&pods);
        for node in &mut nodes {
            node.pod_count = scheduled.get(node.name.as_str()).copied().unwrap_or(0);
        }
        let deployments: Vec<DeploymentSnapshot> = resources
            .deployments
            .iter()
            .map(|d| deployment_to_snapshot(d, now_ms))
            .collect();

        // Partial RBAC access remains usable; a fully failed poll must not
        // advertise the previous successful cache as a live connection.
        let reachable =
            resources.last_update_ms > 0 && resources.poll_state != Some(ResourcePollState::Failed);

        Ok(KubeSnapshot {
            cluster_kind: self.cluster_kind,
            server_version: self.server_version.clone(),
            current_namespace: scope.scope.label().to_string(),
            reachable,
            metrics_available: metrics.available,
            pods,
            nodes,
            deployments,
        })
    }

    async fn metrics_available(&self) -> bool {
        self.metrics.read().await.available
    }

    fn kind(&self) -> ClusterKind {
        self.cluster_kind
    }

    fn server_version(&self) -> Option<&str> {
        self.server_version.as_deref()
    }

    async fn scope(&self) -> KubeScope {
        self.scope.read().await.scope.clone()
    }

    async fn toggle_scope(&self) -> KubeScope {
        let next = match &self.scope.read().await.scope {
            KubeScope::AllNamespaces => KubeScope::Namespace(self.scoped_namespace.clone()),
            KubeScope::Namespace(_) => KubeScope::AllNamespaces,
        };
        self.set_scope(next.clone()).await;
        next
    }
}

// ---- Conversions ---------------------------------------------------------

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Convert a typed [`Pod`] to a wire [`PodSnapshot`], merging in metrics
/// from `metrics` when present.
pub(crate) fn pod_to_snapshot(pod: &Pod, metrics: &MetricsCache, now_ms: u64) -> PodSnapshot {
    let namespace = pod.metadata.namespace.clone().unwrap_or_default();
    let name = pod.metadata.name.clone().unwrap_or_default();
    let phase = pod_phase_synthetic(pod);
    let ready = pod_ready_ratio(pod);
    let restarts = pod_restart_count(pod);
    let age_seconds = creation_age_seconds(pod.metadata.creation_timestamp.as_ref(), now_ms);
    let node = pod
        .spec
        .as_ref()
        .and_then(|s| s.node_name.clone())
        .unwrap_or_default();
    let qos = pod_qos(pod);

    let metrics_key = (namespace.clone(), name.clone());
    let (cpu_millis, mem_bytes) = match metrics.pods.get(&metrics_key) {
        Some((cpu, mem)) => (Some(*cpu), Some(*mem)),
        None => (None, None),
    };

    PodSnapshot {
        namespace,
        name,
        phase,
        ready,
        restarts,
        age_seconds,
        node,
        cpu_millis,
        mem_bytes,
        qos,
    }
}

fn pod_phase_synthetic(pod: &Pod) -> PodPhase {
    // Terminating wins over everything else — once metadata.deletionTimestamp
    // is set, the pod is going away regardless of its container states.
    if pod.metadata.deletion_timestamp.is_some() {
        return PodPhase::Terminating;
    }

    // CrashLoop synthesis: any container with state.waiting.reason ==
    // "CrashLoopBackOff".
    if let Some(status) = &pod.status
        && let Some(statuses) = &status.container_statuses
        && statuses.iter().any(|cs| {
            cs.state
                .as_ref()
                .and_then(|s| s.waiting.as_ref())
                .and_then(|w| w.reason.as_deref())
                == Some("CrashLoopBackOff")
        })
    {
        return PodPhase::CrashLoop;
    }

    match pod
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("")
    {
        "Pending" => PodPhase::Pending,
        "Running" => PodPhase::Running,
        "Succeeded" => PodPhase::Succeeded,
        "Failed" => PodPhase::Failed,
        _ => PodPhase::Unknown,
    }
}

fn pod_ready_ratio(pod: &Pod) -> (u8, u8) {
    let statuses = pod
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref());
    match statuses {
        Some(list) => {
            let total = list.len().min(u8::MAX as usize) as u8;
            let ready = list
                .iter()
                .filter(|cs| cs.ready)
                .count()
                .min(u8::MAX as usize) as u8;
            (ready, total)
        }
        None => (0, 0),
    }
}

fn pod_restart_count(pod: &Pod) -> u32 {
    pod.status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref())
        .map(|list| list.iter().map(|cs| cs.restart_count.max(0) as u32).sum())
        .unwrap_or(0)
}

fn pod_qos(pod: &Pod) -> QosClass {
    match pod
        .status
        .as_ref()
        .and_then(|s| s.qos_class.as_deref())
        .unwrap_or("")
    {
        "Guaranteed" => QosClass::Guaranteed,
        "Burstable" => QosClass::Burstable,
        _ => QosClass::BestEffort,
    }
}

/// Convert a typed [`Node`] to a wire [`NodeSnapshot`].
pub(crate) fn node_to_snapshot(node: &Node, metrics: &MetricsCache, now_ms: u64) -> NodeSnapshot {
    let name = node.metadata.name.clone().unwrap_or_default();
    let status = node_status_synthetic(node);
    let roles = node_roles(node);
    let age_seconds = creation_age_seconds(node.metadata.creation_timestamp.as_ref(), now_ms);
    let kubelet_version = node
        .status
        .as_ref()
        .and_then(|s| s.node_info.as_ref())
        .map(|info| info.kubelet_version.clone())
        .unwrap_or_default();

    let (cpu_capacity_millis, mem_capacity_bytes, pod_capacity) = node
        .status
        .as_ref()
        .and_then(|s| s.capacity.as_ref())
        .map(|caps| {
            let cpu = caps
                .get("cpu")
                .map(|q| parse_quantity_to_millis(&q.0))
                .unwrap_or(0);
            let mem = caps
                .get("memory")
                .map(|q| parse_quantity_to_bytes(&q.0))
                .unwrap_or(0);
            let pods = caps
                .get("pods")
                .and_then(|q| q.0.parse::<u32>().ok())
                .unwrap_or(0);
            (cpu, mem, pods)
        })
        .unwrap_or((0, 0, 0));

    let (cpu_allocatable_millis, mem_allocatable_bytes) = node
        .status
        .as_ref()
        .and_then(|s| s.allocatable.as_ref())
        .map(|alloc| {
            let cpu = alloc
                .get("cpu")
                .map(|q| parse_quantity_to_millis(&q.0))
                .unwrap_or(0);
            let mem = alloc
                .get("memory")
                .map(|q| parse_quantity_to_bytes(&q.0))
                .unwrap_or(0);
            (cpu, mem)
        })
        .unwrap_or((0, 0));

    let (cpu_used_millis, mem_used_bytes) = match metrics.nodes.get(&name) {
        Some((cpu, mem)) => (Some(*cpu), Some(*mem)),
        None => (None, None),
    };

    NodeSnapshot {
        name,
        status,
        roles,
        age_seconds,
        kubelet_version,
        cpu_capacity_millis,
        cpu_allocatable_millis,
        cpu_used_millis,
        mem_capacity_bytes,
        mem_allocatable_bytes,
        mem_used_bytes,
        // Filled in by `ClusterEngine::snapshot`, which has the pod list a
        // lone `Node` object cannot see.
        pod_count: 0,
        pod_capacity,
    }
}

/// Count scheduled pods per node name.
///
/// Mirrors what `kubectl describe node` reports as "Non-terminated Pods":
/// `Succeeded` and `Failed` pods have released their node resources, so
/// counting them would overstate how full a node is — the column exists to
/// be read against `pod_capacity`.
fn pods_per_node(pods: &[PodSnapshot]) -> HashMap<&str, u32> {
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for pod in pods {
        if pod.node.is_empty() || matches!(pod.phase, PodPhase::Succeeded | PodPhase::Failed) {
            continue;
        }
        *counts.entry(pod.node.as_str()).or_insert(0) += 1;
    }
    counts
}

fn node_status_synthetic(node: &Node) -> NodeStatus {
    if node.spec.as_ref().and_then(|s| s.unschedulable) == Some(true) {
        return NodeStatus::SchedulingDisabled;
    }
    let conditions = node.status.as_ref().and_then(|s| s.conditions.as_ref());
    if let Some(conditions) = conditions {
        for cond in conditions {
            if cond.type_ == "Ready" {
                return match cond.status.as_str() {
                    "True" => NodeStatus::Ready,
                    "False" => NodeStatus::NotReady,
                    _ => NodeStatus::Unknown,
                };
            }
        }
    }
    NodeStatus::Unknown
}

fn node_roles(node: &Node) -> Vec<String> {
    let mut roles = Vec::new();
    if let Some(labels) = &node.metadata.labels {
        for k in labels.keys() {
            if let Some(role) = k.strip_prefix("node-role.kubernetes.io/")
                && !role.is_empty()
            {
                roles.push(role.to_string());
            }
        }
    }
    roles.sort();
    roles
}

/// Convert a typed [`Deployment`] to a wire [`DeploymentSnapshot`].
pub(crate) fn deployment_to_snapshot(d: &Deployment, now_ms: u64) -> DeploymentSnapshot {
    let namespace = d.metadata.namespace.clone().unwrap_or_default();
    let name = d.metadata.name.clone().unwrap_or_default();
    let age_seconds = creation_age_seconds(d.metadata.creation_timestamp.as_ref(), now_ms);

    let replicas_desired = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0).max(0) as u32;

    let (replicas_ready, replicas_uptodate, replicas_available) = d
        .status
        .as_ref()
        .map(|s| {
            (
                s.ready_replicas.unwrap_or(0).max(0) as u32,
                s.updated_replicas.unwrap_or(0).max(0) as u32,
                s.available_replicas.unwrap_or(0).max(0) as u32,
            )
        })
        .unwrap_or((0, 0, 0));

    let strategy = d
        .spec
        .as_ref()
        .and_then(|s| s.strategy.as_ref())
        .and_then(|st| st.type_.as_deref())
        .map(|t| match t {
            "Recreate" => DeploymentStrategy::Recreate,
            _ => DeploymentStrategy::RollingUpdate,
        })
        .unwrap_or(DeploymentStrategy::RollingUpdate);

    DeploymentSnapshot {
        namespace,
        name,
        replicas_desired,
        replicas_ready,
        replicas_uptodate,
        replicas_available,
        age_seconds,
        strategy,
    }
}

// ---- Quantity parsing ----------------------------------------------------

/// Parse a Kubernetes `Quantity` string into milli-cores.
///
/// Inputs we accept:
/// * `"4"` → 4_000 (4 cores)
/// * `"2000m"` → 2_000
/// * `"100m"` → 100
/// * `"0.5"` → 500
/// * Anything else → 0 (logged at the call site if needed).
pub(crate) fn parse_quantity_to_millis(raw: &str) -> u32 {
    let s = raw.trim();
    if let Some(stripped) = s.strip_suffix('m') {
        return stripped.parse::<u32>().unwrap_or(0);
    }
    if let Ok(int) = s.parse::<u32>() {
        return int.saturating_mul(1000);
    }
    if let Ok(float) = s.parse::<f64>() {
        return (float * 1000.0).round() as u32;
    }
    0
}

/// Parse a Kubernetes `Quantity` string into bytes.
///
/// Suffixes recognised: `Ki`, `Mi`, `Gi`, `Ti` (binary IEC) and `K`, `M`,
/// `G`, `T` (decimal SI). `n`/`u`/`m` (sub-unit) are intentionally not
/// supported — they don't appear in capacity/allocatable for memory.
pub(crate) fn parse_quantity_to_bytes(raw: &str) -> u64 {
    let s = raw.trim();
    let multipliers: &[(&str, u64)] = &[
        ("Ti", 1u64 << 40),
        ("Gi", 1u64 << 30),
        ("Mi", 1u64 << 20),
        ("Ki", 1u64 << 10),
        ("T", 1_000_000_000_000),
        ("G", 1_000_000_000),
        ("M", 1_000_000),
        ("K", 1_000),
    ];
    for (suffix, mult) in multipliers {
        if let Some(stripped) = s.strip_suffix(suffix) {
            return stripped
                .parse::<u64>()
                .map(|n| n.saturating_mul(*mult))
                .unwrap_or(0);
        }
    }
    s.parse::<u64>().unwrap_or(0)
}

// ---- Time helpers --------------------------------------------------------

fn creation_age_seconds(
    creation: Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,
    now_ms: u64,
) -> u64 {
    let Some(t) = creation else { return 0 };
    let created_ms = t.0.as_millisecond();
    if created_ms <= 0 {
        return 0;
    }
    let created_ms = created_ms as u64;
    now_ms.saturating_sub(created_ms) / 1000
}

// ---- Connect helpers ----------------------------------------------------

/// Build a [`kube::Config`] from a [`KubeconfigSource`] + optional context
/// and namespace. Maps every kube error to a [`ClusterError::Unreachable`]
/// (or [`ClusterError::KubeconfigNotFound`] for `Source::None`).
async fn build_config(
    source: KubeconfigSource,
    context: Option<&str>,
    namespace: Option<&str>,
) -> Result<Config, ClusterError> {
    let mut config = match source {
        KubeconfigSource::None => Err(ClusterError::KubeconfigNotFound),
        source @ (KubeconfigSource::Env(_) | KubeconfigSource::Home(_)) => {
            let paths = match source {
                KubeconfigSource::Env(path) => std::env::split_paths(path.as_os_str())
                    .filter(|path| !path.as_os_str().is_empty())
                    .collect::<Vec<_>>(),
                KubeconfigSource::Home(path) => vec![path],
                _ => unreachable!("source was matched above"),
            };
            let kc = paths
                .into_iter()
                .try_fold(Kubeconfig::default(), |merged, path| {
                    Kubeconfig::read_from(&path)
                        .and_then(|next| merged.merge(next))
                        .map_err(|error| {
                            ClusterError::Unreachable(format!(
                                "read kubeconfig {}: {error}",
                                path.display()
                            ))
                        })
                })?;
            let opts = KubeConfigOptions {
                context: context.map(String::from),
                cluster: None,
                user: None,
            };
            let mut cfg = Config::from_custom_kubeconfig(kc, &opts)
                .await
                .map_err(|e| ClusterError::Unreachable(format!("apply kubeconfig: {e}")))?;
            if let Some(ns) = namespace {
                cfg.default_namespace = ns.to_string();
            }
            Ok(cfg)
        }
        KubeconfigSource::InCluster => {
            let mut cfg = Config::incluster()
                .map_err(|e| ClusterError::Unreachable(format!("in-cluster config: {e}")))?;
            if let Some(ns) = namespace {
                cfg.default_namespace = ns.to_string();
            }
            Ok(cfg)
        }
    }?;
    // Our poll loops own retry cadence and wrap each request in LIST_TIMEOUT.
    // kube 4 otherwise retries transient statuses inside a single collection.
    config.default_retry = false;
    Ok(config)
}

/// Probe the API server's `/version` endpoint and derive a [`ClusterKind`]
/// plus version string from the response.
///
/// Returns `Err` on any transport or parse failure; callers fall back to
/// [`ClusterKind::Generic`] and `None`.
async fn probe_version(client: &Client) -> Result<(ClusterKind, String), ClusterError> {
    let req = http::Request::builder()
        .uri("/version")
        .method("GET")
        .body(Vec::new())
        .map_err(|e| ClusterError::Other(format!("/version build: {e}")))?;
    let body = tokio::time::timeout(LIST_TIMEOUT, client.request_text(req))
        .await
        .map_err(|_| {
            ClusterError::Unreachable(format!("/version timed out after {LIST_TIMEOUT:?}"))
        })?
        .map_err(|e| ClusterError::Unreachable(format!("/version: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| ClusterError::Other(format!("/version parse: {e}")))?;
    let git_version = v
        .get("gitVersion")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let kind = cluster_kind_from_git_version(&git_version);
    Ok((kind, git_version))
}

// ---- Resource poll loop -------------------------------------------------

/// Spawn the 5 s loop that lists Pods / Nodes / Deployments. Per-resource
/// failures are logged while successful kinds remain usable under restricted
/// RBAC. Failed kinds are omitted instead of advertising old rows as live.
fn spawn_resource_loop(
    client: Client,
    cache: Arc<RwLock<ResourceCache>>,
    scope: Arc<RwLock<ScopeState>>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // `Node` is cluster-scoped in Kubernetes — no namespaced variant
        // exists — so this Api is built once and never rescoped. Under a
        // namespace-only Role the list 403s and the Nodes sub-view degrades
        // to empty, which is the documented behaviour.
        let node_api: Api<Node> = Api::all(client.clone());
        let lp = ListParams::default().limit(5_000);

        // `interval` rather than tick-then-sleep: the latter made the real
        // period `tick duration + POLL_INTERVAL`, so the loop drifted away
        // from the 5 s the collector samples at and the two fell out of
        // phase. `Skip` keeps a slow tick from being chased by a burst of
        // catch-up polls. Same shape as the collector and container loops.
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }

            // Rebuilt every tick because `A` can rescope the engine between
            // ticks. `Api::all` / `Api::namespaced` only wrap the shared
            // client in a request builder — no I/O, no handshake, so this is
            // cheaper than any change-detection scheme would be.
            let current = scope.read().await.clone();
            let (pod_api, deployment_api) = match current.scope.namespace() {
                Some(ns) => (
                    Api::<Pod>::namespaced(client.clone(), ns),
                    Api::<Deployment>::namespaced(client.clone(), ns),
                ),
                None => (
                    Api::<Pod>::all(client.clone()),
                    Api::<Deployment>::all(client.clone()),
                ),
            };

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tick_resources(&pod_api, &node_api, &deployment_api, &lp, &cache, &scope, current.generation) => {}
            }
        }
    })
}

/// List one resource kind, returning `None` on timeout or API error.
///
/// `None` identifies an unavailable kind. A failure must not wipe the other
/// two kinds: namespace-only RBAC commonly denies cluster-scoped Nodes.
async fn list_or_warn<K>(api: &Api<K>, lp: &ListParams, kind: &'static str) -> Option<Vec<K>>
where
    K: Clone + std::fmt::Debug + serde::de::DeserializeOwned,
{
    match tokio::time::timeout(LIST_TIMEOUT, api.list(lp)).await {
        Ok(Ok(list)) => Some(list.items),
        Ok(Err(e)) => {
            tracing::warn!(target: "muxtop::kube", kind, error = %e, "list failed");
            None
        }
        Err(_) => {
            tracing::warn!(target: "muxtop::kube", kind, "list timed out");
            None
        }
    }
}

async fn tick_resources(
    pod_api: &Api<Pod>,
    node_api: &Api<Node>,
    deployment_api: &Api<Deployment>,
    lp: &ListParams,
    cache: &Arc<RwLock<ResourceCache>>,
    scope: &Arc<RwLock<ScopeState>>,
    generation: u64,
) {
    // PERF-04: run the three lists concurrently. Sequentially they could
    // spend 3 x LIST_TIMEOUT = 9 s on a slow API server — longer than the
    // 5 s POLL_INTERVAL, so the loop would fall behind its own cadence and
    // the UI would show data older than it advertises. Concurrently the
    // worst case is one LIST_TIMEOUT, comfortably inside the interval.
    //
    // `join!` polls on this task rather than spawning: no 'static bounds,
    // and cancellation still propagates through the caller's `select!`.
    let (pods, nodes, deployments) = tokio::join!(
        list_or_warn(pod_api, lp, "pods"),
        list_or_warn(node_api, lp, "nodes"),
        list_or_warn(deployment_api, lp, "deployments"),
    );

    let current = scope.read().await;
    if current.generation != generation {
        return;
    }
    let succeeded = usize::from(pods.is_some())
        + usize::from(nodes.is_some())
        + usize::from(deployments.is_some());
    let mut w = cache.write().await;
    w.poll_state = Some(match succeeded {
        0 => ResourcePollState::Failed,
        3 => ResourcePollState::Complete,
        _ => ResourcePollState::Partial,
    });
    if succeeded == 0 {
        return;
    }
    // Empty inaccessible kinds rather than retaining rows that look live.
    w.pods = pods.unwrap_or_default();
    w.nodes = nodes.unwrap_or_default();
    w.deployments = deployments.unwrap_or_default();
    w.last_update_ms = unix_ms();
    w.last_success = Some(tokio::time::Instant::now());
    if succeeded < 3 {
        tracing::debug!(target: "muxtop::kube", succeeded, "partial resource access; inaccessible lists omitted");
    }
}

// ---- Metrics poll loop --------------------------------------------------

/// Spawn the 5 s loop that polls `/apis/metrics.k8s.io/v1beta1/{pods,nodes}`.
fn spawn_metrics_loop(
    client: Client,
    cache: Arc<RwLock<MetricsCache>>,
    scope: Arc<RwLock<ScopeState>>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // See `spawn_resource_loop` for why this is an interval rather than
        // tick-then-sleep.
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }
            // Re-read after the tick: `A` can rescope the engine while we
            // wait, and the poll must use the scope in force now.
            let current = scope.read().await.clone();
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tick_metrics(&client, &cache, &current.scope, &scope, current.generation) => {}
            }
        }
    })
}

async fn tick_metrics(
    client: &Client,
    cache: &Arc<RwLock<MetricsCache>>,
    scope: &KubeScope,
    live_scope: &Arc<RwLock<ScopeState>>,
    generation: u64,
) {
    // Pod metrics follow the resource scope: a namespace-only Role can read
    // `namespaces/{ns}/pods` but not the cluster-wide collection, so an
    // unscoped poll here would 403 and blank the CPU/MEM columns for exactly
    // the users namespace scoping exists to serve.
    //
    // `ns` is a validated DNS-1123 label (see `is_valid_namespace`), so it
    // cannot introduce path segments or a query string.
    let pod_path = match scope.namespace() {
        Some(ns) => format!("/apis/metrics.k8s.io/v1beta1/namespaces/{ns}/pods"),
        None => "/apis/metrics.k8s.io/v1beta1/pods".to_string(),
    };
    // Node metrics are cluster-scoped like the Node resource itself.
    //
    // PERF-04: same reasoning as `tick_resources` — sequentially these two
    // fetches could take 2 x LIST_TIMEOUT = 6 s against a 5 s POLL_INTERVAL.
    let (pod_metrics, node_metrics) = tokio::join!(
        fetch_metrics_text(client, &pod_path),
        fetch_metrics_text(client, "/apis/metrics.k8s.io/v1beta1/nodes"),
    );

    let current = live_scope.read().await;
    if current.generation != generation {
        return;
    }

    // Treat any error on either path as "metrics-server unavailable". This
    // matches what k9s does — the user just sees `—` in the CPU/MEM cols.
    // A scoped engine that can read pod metrics but not node metrics keeps
    // `available = true` — the pod columns populate and the node ones don't.
    if pod_metrics.is_none() && node_metrics.is_none() {
        let mut w = cache.write().await;
        w.available = false;
        w.pods.clear();
        w.nodes.clear();
        return;
    }

    let mut new_pods: HashMap<(String, String), (u32, u64)> = HashMap::new();
    if let Some(text) = &pod_metrics
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
        && let Some(items) = v.get("items").and_then(|x| x.as_array())
    {
        for item in items {
            let ns = item
                .pointer("/metadata/namespace")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let name = item
                .pointer("/metadata/name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            // Sum cpu + mem across containers for the pod.
            let mut cpu_total: u32 = 0;
            let mut mem_total: u64 = 0;
            if let Some(containers) = item.get("containers").and_then(|x| x.as_array()) {
                for c in containers {
                    if let Some(cpu) = c.pointer("/usage/cpu").and_then(|x| x.as_str()) {
                        cpu_total = cpu_total.saturating_add(parse_metrics_cpu_to_millis(cpu));
                    }
                    if let Some(mem) = c.pointer("/usage/memory").and_then(|x| x.as_str()) {
                        mem_total = mem_total.saturating_add(parse_quantity_to_bytes(mem));
                    }
                }
            }
            if !ns.is_empty() && !name.is_empty() {
                new_pods.insert((ns, name), (cpu_total, mem_total));
            }
        }
    }

    let mut new_nodes: HashMap<String, (u32, u64)> = HashMap::new();
    if let Some(text) = &node_metrics
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
        && let Some(items) = v.get("items").and_then(|x| x.as_array())
    {
        for item in items {
            let name = item
                .pointer("/metadata/name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let cpu = item
                .pointer("/usage/cpu")
                .and_then(|x| x.as_str())
                .map(parse_metrics_cpu_to_millis)
                .unwrap_or(0);
            let mem = item
                .pointer("/usage/memory")
                .and_then(|x| x.as_str())
                .map(parse_quantity_to_bytes)
                .unwrap_or(0);
            if !name.is_empty() {
                new_nodes.insert(name, (cpu, mem));
            }
        }
    }

    let mut w = cache.write().await;
    w.available = true;
    w.pods = new_pods;
    w.nodes = new_nodes;
}

/// Ceiling on a metrics-server response body (SEC-05).
///
/// Unlike the typed resource lists, `metrics.k8s.io` is served by an
/// *aggregated* APIService: whoever runs that pod — or anyone able to
/// register an APIService for it — chooses the response, and
/// `Client::request_text` would buffer whatever they send. Streaming with a
/// cap turns a hostile or malfunctioning metrics backend into a dropped
/// poll instead of an out-of-memory kill.
///
/// 16 MiB is roughly 4x the largest plausible legitimate body (~4 MiB for
/// the 5 000-pod ceiling `ListParams` imposes on the resource side).
const MAX_METRICS_BODY: u64 = 16 * 1024 * 1024;

async fn fetch_metrics_text(client: &Client, path: &str) -> Option<String> {
    let req = http::Request::builder()
        .uri(path)
        .method("GET")
        .body(Vec::new())
        .ok()?;
    match tokio::time::timeout(LIST_TIMEOUT, read_capped_body(client, req)).await {
        Ok(Ok(body)) => Some(body),
        Ok(Err(e)) => {
            tracing::debug!(target: "muxtop::kube", path, error = %e, "metrics fetch failed");
            None
        }
        Err(_) => {
            tracing::debug!(target: "muxtop::kube", path, "metrics fetch timed out");
            None
        }
    }
}

/// Read a response body, refusing to buffer more than [`MAX_METRICS_BODY`].
///
/// Reads one byte past the cap so an over-long body is detected rather than
/// silently truncated into invalid JSON.
async fn read_capped_body(
    client: &Client,
    req: http::Request<Vec<u8>>,
) -> Result<String, ClusterError> {
    let stream = client
        .request_stream(req)
        .await
        .map_err(|e| ClusterError::Unreachable(e.to_string()))?;
    futures::pin_mut!(stream);
    read_capped(stream, MAX_METRICS_BODY).await
}

/// Buffer at most `cap` bytes from `stream` as UTF-8.
///
/// Reads one byte past `cap` so an over-long body is rejected outright
/// rather than silently truncated into invalid JSON — a truncated parse
/// would look like "metrics unavailable" and hide the real problem.
pub(crate) async fn read_capped<R>(stream: R, cap: u64) -> Result<String, ClusterError>
where
    R: futures::AsyncRead,
{
    use futures::AsyncReadExt;

    let mut buf = Vec::new();
    futures::pin_mut!(stream);
    stream
        .take(cap + 1)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| ClusterError::Other(format!("metrics body read: {e}")))?;

    if buf.len() as u64 > cap {
        return Err(ClusterError::Other(format!(
            "metrics body exceeded {cap} bytes; refusing to buffer it"
        )));
    }

    String::from_utf8(buf).map_err(|e| ClusterError::Other(format!("metrics body utf-8: {e}")))
}

/// metrics-server reports CPU usage in nanocores (`"123456789n"`) most of
/// the time, but occasionally as plain milli (`"100m"`) or core (`"1"`)
/// units depending on the source. Converge on millis.
pub(crate) fn parse_metrics_cpu_to_millis(raw: &str) -> u32 {
    let s = raw.trim();
    if let Some(stripped) = s.strip_suffix('n') {
        // nanocores → millicores: divide by 1_000_000, saturate.
        return stripped
            .parse::<u64>()
            .map(|n| (n / 1_000_000) as u32)
            .unwrap_or(0);
    }
    if let Some(stripped) = s.strip_suffix('u') {
        // microcores → millicores: divide by 1_000.
        return stripped
            .parse::<u64>()
            .map(|n| (n / 1_000) as u32)
            .unwrap_or(0);
    }
    parse_quantity_to_millis(s)
}

// ---- Cluster kind heuristic ---------------------------------------------

/// Derive a [`ClusterKind`] from the API server `gitVersion` string.
///
/// Heuristics are intentionally ASCII-cheap (substring match on the
/// lowercased version string) — false positives are acceptable since
/// `cluster_kind` only drives a UI badge.
pub(crate) fn cluster_kind_from_git_version(git_version: &str) -> ClusterKind {
    let v = git_version.to_ascii_lowercase();
    if v.contains("eks") {
        return ClusterKind::Eks;
    }
    if v.contains("gke") {
        return ClusterKind::Gke;
    }
    if v.contains("aks") {
        return ClusterKind::Aks;
    }
    if v.contains("k3d") {
        return ClusterKind::K3d;
    }
    if v.contains("k3s") {
        return ClusterKind::K3s;
    }
    if v.contains("kind") {
        return ClusterKind::Kind;
    }
    if v.contains("openshift") {
        return ClusterKind::Openshift;
    }
    ClusterKind::Generic
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_engine_for_test() -> KubeEngine {
        KubeEngine::new_for_test(
            ClusterKind::Generic,
            None,
            String::new(),
            ResourceCache::default(),
            MetricsCache::default(),
        )
    }

    // ---- pod_count join ----

    fn pod_on(node: &str, phase: PodPhase) -> PodSnapshot {
        PodSnapshot {
            namespace: "default".into(),
            name: format!("pod-{node}-{phase:?}"),
            phase,
            ready: (1, 1),
            restarts: 0,
            age_seconds: 60,
            node: node.into(),
            cpu_millis: None,
            mem_bytes: None,
            qos: QosClass::BestEffort,
        }
    }

    #[test]
    fn pods_per_node_counts_only_scheduled_live_pods() {
        let pods = vec![
            pod_on("node-a", PodPhase::Running),
            pod_on("node-a", PodPhase::Pending),
            pod_on("node-a", PodPhase::CrashLoop),
            // Terminated: resources already released, so kubectl does not
            // count these against the node either.
            pod_on("node-a", PodPhase::Succeeded),
            pod_on("node-a", PodPhase::Failed),
            pod_on("node-b", PodPhase::Running),
            // Unscheduled pods belong to no node.
            pod_on("", PodPhase::Pending),
        ];

        let counts = pods_per_node(&pods);
        assert_eq!(counts.get("node-a").copied(), Some(3));
        assert_eq!(counts.get("node-b").copied(), Some(1));
        assert_eq!(
            counts.get("").copied(),
            None,
            "unscheduled pods have no node"
        );
    }

    /// Regression for the hardcoded `pod_count: 0` the Nodes column shipped
    /// with in v0.4.0 — it rendered "0" on every cluster.
    #[tokio::test]
    async fn snapshot_populates_node_pod_count() {
        let node: Node = serde_json::from_value(json!({
            "metadata": { "name": "node-a" },
            "status": {
                "conditions": [{ "type": "Ready", "status": "True" }],
                "capacity": { "cpu": "4", "memory": "8Gi", "pods": "110" }
            }
        }))
        .unwrap();
        let pod: Pod = serde_json::from_value(json!({
            "metadata": { "name": "web-0", "namespace": "default" },
            "spec": { "nodeName": "node-a" },
            "status": { "phase": "Running" }
        }))
        .unwrap();

        let engine = KubeEngine::new_for_test(
            ClusterKind::Generic,
            None,
            "default".into(),
            ResourceCache {
                pods: vec![pod],
                nodes: vec![node],
                deployments: Vec::new(),
                last_update_ms: unix_ms(),
                ..Default::default()
            },
            MetricsCache::default(),
        );

        let snap = engine.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.nodes[0].pod_count, 1,
            "the Nodes PODS column must reflect the actual pod list"
        );
        assert_eq!(snap.nodes[0].pod_capacity, 110);
    }

    // ---- SEC-05: bounded metrics body ----

    #[tokio::test]
    async fn read_capped_accepts_a_body_within_the_cap() {
        let body = br#"{"items":[]}"#;
        let out = read_capped(&body[..], 1024).await.expect("within cap");
        assert_eq!(out, r#"{"items":[]}"#);
    }

    #[tokio::test]
    async fn read_capped_accepts_a_body_exactly_at_the_cap() {
        let body = [b'x'; 64];
        let out = read_capped(&body[..], 64).await.expect("cap is inclusive");
        assert_eq!(out.len(), 64);
    }

    /// The finding: an aggregated APIService chooses this response, so an
    /// unbounded body is a memory-exhaustion primitive handed to whoever
    /// runs metrics-server.
    #[tokio::test]
    async fn read_capped_rejects_an_oversized_body() {
        let body = vec![b'x'; 4096];
        let err = read_capped(&body[..], 1024)
            .await
            .expect_err("body is 4x the cap");
        assert!(
            matches!(&err, ClusterError::Other(m) if m.contains("exceeded")),
            "expected an explicit over-cap error, got {err:?}"
        );
    }

    /// Rejecting beats truncating: a half-read body would parse as invalid
    /// JSON and be indistinguishable from "metrics-server is down".
    #[tokio::test]
    async fn read_capped_does_not_truncate_into_valid_looking_output() {
        let body = vec![b'x'; 2048];
        assert!(read_capped(&body[..], 1024).await.is_err());
    }

    // ---- connect stubs ----

    #[tokio::test]
    async fn connect_rejects_kubeconfig_none() {
        let res = KubeEngine::connect(KubeconfigSource::None, None, None).await;
        assert!(matches!(res, Err(ClusterError::KubeconfigNotFound)));
    }

    #[tokio::test]
    async fn connect_with_empty_kubeconfig_is_unreachable() {
        // An empty file isn't valid YAML/kubeconfig — read or apply fails
        // and surfaces as Unreachable (carries the kube-rs error verbatim).
        let dir = tempfile::tempdir().unwrap();
        let kc = dir.path().join("config");
        std::fs::File::create(&kc).unwrap();

        let res = KubeEngine::connect(KubeconfigSource::Home(kc), None, None).await;
        assert!(matches!(res, Err(ClusterError::Unreachable(_))));
    }

    #[tokio::test]
    async fn connect_with_missing_path_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.yaml");
        let res = KubeEngine::connect(KubeconfigSource::Env(missing), None, None).await;
        assert!(matches!(res, Err(ClusterError::Unreachable(_))));
    }

    // ---- empty engine ----

    #[tokio::test]
    async fn empty_engine_yields_unreachable_snapshot() {
        let engine = empty_engine_for_test();
        let snap = engine.snapshot().await.expect("snapshot");
        assert!(!snap.reachable);
        assert!(snap.pods.is_empty());
        assert!(snap.nodes.is_empty());
        assert!(snap.deployments.is_empty());
        assert!(!snap.metrics_available);
    }

    #[tokio::test]
    async fn engine_implements_cluster_engine_trait() {
        let engine: Box<dyn ClusterEngine + Send + Sync> = Box::new(empty_engine_for_test());
        assert_eq!(engine.kind(), ClusterKind::Generic);
        assert!(engine.server_version().is_none());
        assert!(!engine.metrics_available().await);
    }

    #[tokio::test]
    async fn drop_cancels_token() {
        let engine = empty_engine_for_test();
        let token = engine.cancel.clone();
        assert!(!token.is_cancelled());
        drop(engine);
        assert!(token.is_cancelled());
    }

    // ---- pod conversions ----

    fn pod_from_json(value: serde_json::Value) -> Pod {
        serde_json::from_value(value).expect("valid Pod JSON")
    }

    #[test]
    fn pod_to_snapshot_running() {
        let p = pod_from_json(json!({
            "metadata": { "namespace": "default", "name": "nginx-1" },
            "spec": { "nodeName": "node-1" },
            "status": {
                "phase": "Running",
                "qosClass": "Burstable",
                "containerStatuses": [
                    { "name": "main", "ready": true, "restartCount": 0,
                      "image": "nginx:1.27", "imageID": "", "state": {"running": {}} }
                ]
            }
        }));
        let metrics = MetricsCache::default();
        let snap = pod_to_snapshot(&p, &metrics, 0);
        assert_eq!(snap.namespace, "default");
        assert_eq!(snap.name, "nginx-1");
        assert_eq!(snap.phase, PodPhase::Running);
        assert_eq!(snap.ready, (1, 1));
        assert_eq!(snap.restarts, 0);
        assert_eq!(snap.node, "node-1");
        assert_eq!(snap.qos, QosClass::Burstable);
        assert!(snap.cpu_millis.is_none());
        assert!(snap.mem_bytes.is_none());
    }

    #[test]
    fn pod_to_snapshot_crashloop_synth() {
        let p = pod_from_json(json!({
            "metadata": { "namespace": "default", "name": "broken" },
            "status": {
                "phase": "Running",
                "containerStatuses": [
                    { "name": "main", "ready": false, "restartCount": 7,
                      "image": "x", "imageID": "",
                      "state": { "waiting": { "reason": "CrashLoopBackOff" } } }
                ]
            }
        }));
        let metrics = MetricsCache::default();
        let snap = pod_to_snapshot(&p, &metrics, 0);
        assert_eq!(snap.phase, PodPhase::CrashLoop);
        assert_eq!(snap.restarts, 7);
        assert_eq!(snap.ready, (0, 1));
    }

    #[test]
    fn pod_to_snapshot_terminating_synth() {
        let p = pod_from_json(json!({
            "metadata": {
                "namespace": "default",
                "name": "going-away",
                "deletionTimestamp": "2026-04-26T00:00:00Z"
            },
            "status": { "phase": "Running" }
        }));
        let metrics = MetricsCache::default();
        let snap = pod_to_snapshot(&p, &metrics, 0);
        assert_eq!(snap.phase, PodPhase::Terminating);
    }

    #[test]
    fn pod_to_snapshot_metrics_injection() {
        let p = pod_from_json(json!({
            "metadata": { "namespace": "default", "name": "instrumented" },
            "status": { "phase": "Running" }
        }));
        let mut pods = HashMap::new();
        pods.insert(
            ("default".into(), "instrumented".into()),
            (42, 128 * 1024 * 1024),
        );
        let metrics = MetricsCache {
            available: true,
            pods,
            ..Default::default()
        };
        let snap = pod_to_snapshot(&p, &metrics, 0);
        assert_eq!(snap.cpu_millis, Some(42));
        assert_eq!(snap.mem_bytes, Some(128 * 1024 * 1024));
    }

    #[test]
    fn pod_to_snapshot_unknown_phase_falls_through() {
        let p = pod_from_json(json!({
            "metadata": { "namespace": "x", "name": "y" },
            "status": { "phase": "WeirdPhase" }
        }));
        let metrics = MetricsCache::default();
        let snap = pod_to_snapshot(&p, &metrics, 0);
        assert_eq!(snap.phase, PodPhase::Unknown);
    }

    // ---- node conversions ----

    fn node_from_json(value: serde_json::Value) -> Node {
        serde_json::from_value(value).expect("valid Node JSON")
    }

    #[test]
    fn node_to_snapshot_basic() {
        let n = node_from_json(json!({
            "metadata": {
                "name": "node-1",
                "labels": {
                    "node-role.kubernetes.io/control-plane": "",
                    "node-role.kubernetes.io/worker": "",
                    "kubernetes.io/hostname": "node-1"
                }
            },
            "spec": {},
            "status": {
                "capacity": { "cpu": "4", "memory": "8Gi", "pods": "110" },
                "allocatable": { "cpu": "3800m", "memory": "7900Mi", "pods": "110" },
                "conditions": [
                    { "type": "Ready", "status": "True", "lastTransitionTime": "2026-04-26T00:00:00Z", "lastHeartbeatTime": "2026-04-26T00:00:00Z" }
                ],
                "nodeInfo": {
                    "kubeletVersion": "v1.31.0",
                    "architecture": "amd64",
                    "bootID": "",
                    "containerRuntimeVersion": "containerd://1.7",
                    "kernelVersion": "6.1",
                    "kubeProxyVersion": "v1.31.0",
                    "machineID": "",
                    "operatingSystem": "linux",
                    "osImage": "linux",
                    "systemUUID": ""
                }
            }
        }));
        let metrics = MetricsCache::default();
        let snap = node_to_snapshot(&n, &metrics, 0);
        assert_eq!(snap.name, "node-1");
        assert_eq!(snap.status, NodeStatus::Ready);
        assert_eq!(
            snap.roles,
            vec!["control-plane".to_string(), "worker".to_string()]
        );
        assert_eq!(snap.kubelet_version, "v1.31.0");
        assert_eq!(snap.cpu_capacity_millis, 4_000);
        assert_eq!(snap.cpu_allocatable_millis, 3_800);
        assert_eq!(snap.mem_capacity_bytes, 8u64 * 1024 * 1024 * 1024);
        assert_eq!(snap.mem_allocatable_bytes, 7_900u64 * 1024 * 1024);
        assert_eq!(snap.pod_capacity, 110);
        assert!(snap.cpu_used_millis.is_none());
    }

    #[test]
    fn node_unschedulable_is_scheduling_disabled() {
        let n = node_from_json(json!({
            "metadata": { "name": "node-x" },
            "spec": { "unschedulable": true },
            "status": {
                "conditions": [
                    { "type": "Ready", "status": "True", "lastTransitionTime": "2026-04-26T00:00:00Z", "lastHeartbeatTime": "2026-04-26T00:00:00Z" }
                ]
            }
        }));
        let metrics = MetricsCache::default();
        let snap = node_to_snapshot(&n, &metrics, 0);
        assert_eq!(snap.status, NodeStatus::SchedulingDisabled);
    }

    #[test]
    fn node_metrics_injection() {
        let n = node_from_json(json!({
            "metadata": { "name": "node-1" },
            "spec": {},
            "status": {}
        }));
        let mut nodes = HashMap::new();
        nodes.insert("node-1".into(), (420, 2u64 * 1024 * 1024 * 1024));
        let metrics = MetricsCache {
            available: true,
            nodes,
            ..Default::default()
        };
        let snap = node_to_snapshot(&n, &metrics, 0);
        assert_eq!(snap.cpu_used_millis, Some(420));
        assert_eq!(snap.mem_used_bytes, Some(2u64 * 1024 * 1024 * 1024));
    }

    // ---- deployment conversions ----

    fn deployment_from_json(value: serde_json::Value) -> Deployment {
        serde_json::from_value(value).expect("valid Deployment JSON")
    }

    #[test]
    fn deployment_to_snapshot_basic() {
        let d = deployment_from_json(json!({
            "metadata": { "namespace": "default", "name": "nginx" },
            "spec": {
                "replicas": 3,
                "selector": { "matchLabels": { "app": "nginx" } },
                "strategy": { "type": "RollingUpdate" },
                "template": { "metadata": {}, "spec": { "containers": [] } }
            },
            "status": {
                "readyReplicas": 3,
                "updatedReplicas": 3,
                "availableReplicas": 3
            }
        }));
        let snap = deployment_to_snapshot(&d, 0);
        assert_eq!(snap.namespace, "default");
        assert_eq!(snap.name, "nginx");
        assert_eq!(snap.replicas_desired, 3);
        assert_eq!(snap.replicas_ready, 3);
        assert_eq!(snap.replicas_uptodate, 3);
        assert_eq!(snap.replicas_available, 3);
        assert_eq!(snap.strategy, DeploymentStrategy::RollingUpdate);
    }

    #[test]
    fn deployment_to_snapshot_recreate_strategy() {
        let d = deployment_from_json(json!({
            "metadata": { "namespace": "default", "name": "rec" },
            "spec": {
                "replicas": 1,
                "selector": { "matchLabels": { "a": "b" } },
                "strategy": { "type": "Recreate" },
                "template": { "metadata": {}, "spec": { "containers": [] } }
            },
            "status": {}
        }));
        let snap = deployment_to_snapshot(&d, 0);
        assert_eq!(snap.strategy, DeploymentStrategy::Recreate);
    }

    // ---- quantity parsing ----

    #[test]
    fn parse_quantity_millis_cases() {
        assert_eq!(parse_quantity_to_millis("4"), 4_000);
        assert_eq!(parse_quantity_to_millis("2000m"), 2_000);
        assert_eq!(parse_quantity_to_millis("100m"), 100);
        assert_eq!(parse_quantity_to_millis("0.5"), 500);
        assert_eq!(parse_quantity_to_millis("1.5"), 1_500);
        assert_eq!(parse_quantity_to_millis(""), 0);
        assert_eq!(parse_quantity_to_millis("garbage"), 0);
    }

    #[test]
    fn parse_metrics_cpu_cases() {
        // metrics-server reports nanocores most of the time.
        assert_eq!(parse_metrics_cpu_to_millis("123456789n"), 123); // 123 ms
        assert_eq!(parse_metrics_cpu_to_millis("1000000000n"), 1_000); // 1 core
        assert_eq!(parse_metrics_cpu_to_millis("500u"), 0); // 0.5 ms rounded down
        assert_eq!(parse_metrics_cpu_to_millis("100m"), 100); // already millis
        assert_eq!(parse_metrics_cpu_to_millis("2"), 2_000); // 2 cores
        assert_eq!(parse_metrics_cpu_to_millis(""), 0);
        assert_eq!(parse_metrics_cpu_to_millis("garbage"), 0);
    }

    #[test]
    fn parse_quantity_bytes_cases() {
        assert_eq!(parse_quantity_to_bytes("8Gi"), 8u64 * 1024 * 1024 * 1024);
        assert_eq!(parse_quantity_to_bytes("7900Mi"), 7_900u64 * 1024 * 1024);
        assert_eq!(parse_quantity_to_bytes("1Ti"), 1u64 << 40);
        assert_eq!(parse_quantity_to_bytes("1024Ki"), 1_024 * 1_024);
        assert_eq!(parse_quantity_to_bytes("1G"), 1_000_000_000);
        assert_eq!(parse_quantity_to_bytes("1024"), 1_024);
        assert_eq!(parse_quantity_to_bytes(""), 0);
        assert_eq!(parse_quantity_to_bytes("garbage"), 0);
    }

    // ---- cluster kind heuristic ----

    #[test]
    fn cluster_kind_from_git_version_cases() {
        assert_eq!(
            cluster_kind_from_git_version("v1.31.0"),
            ClusterKind::Generic
        );
        assert_eq!(
            cluster_kind_from_git_version("v1.31.0-eks-abcd123"),
            ClusterKind::Eks
        );
        assert_eq!(
            cluster_kind_from_git_version("v1.31.0-gke.1700"),
            ClusterKind::Gke
        );
        assert_eq!(
            cluster_kind_from_git_version("v1.30.0+aks"),
            ClusterKind::Aks
        );
        assert_eq!(
            cluster_kind_from_git_version("v1.30.0+k3s1"),
            ClusterKind::K3s
        );
        assert_eq!(
            cluster_kind_from_git_version("v1.31.0-kind"),
            ClusterKind::Kind
        );
        assert_eq!(
            cluster_kind_from_git_version("v1.27.0+openshift"),
            ClusterKind::Openshift
        );
    }

    // ---- end-to-end snapshot ----

    #[tokio::test]
    async fn snapshot_with_populated_caches_is_reachable() {
        let pod = pod_from_json(json!({
            "metadata": { "namespace": "default", "name": "hello" },
            "spec": {},
            "status": { "phase": "Running" }
        }));
        let resources = ResourceCache {
            pods: vec![pod],
            last_update_ms: unix_ms(),
            ..Default::default()
        };

        let metrics = MetricsCache {
            available: true,
            ..Default::default()
        };

        let engine = KubeEngine::new_for_test(
            ClusterKind::Kind,
            Some("v1.31.0".into()),
            "default".into(),
            resources,
            metrics,
        );
        let snap = engine.snapshot().await.unwrap();
        assert!(snap.reachable);
        assert!(snap.metrics_available);
        assert_eq!(snap.pods.len(), 1);
        assert_eq!(snap.cluster_kind, ClusterKind::Kind);
        assert_eq!(snap.server_version.as_deref(), Some("v1.31.0"));
        assert_eq!(snap.current_namespace, "default");
    }

    // ─── namespace scoping ───────────────────────────────────────────────

    #[tokio::test]
    async fn connect_rejects_invalid_namespace_before_touching_the_network() {
        // Validation must run ahead of `build_config`, so an invalid
        // namespace reports *why* instead of a misleading config error.
        // `Source::None` would otherwise yield `KubeconfigNotFound`.
        // `KubeEngine` is not `Debug`, so this cannot use `expect_err`.
        match KubeEngine::connect(KubeconfigSource::None, None, Some("Bad/NS")).await {
            Err(ClusterError::Other(msg)) => {
                assert!(msg.contains("DNS-1123"), "unhelpful message: {msg}");
                assert!(msg.contains("Bad/NS"), "message omits the input: {msg}");
            }
            Err(other) => panic!("expected ClusterError::Other, got {other:?}"),
            Ok(_) => panic!("invalid namespace must be rejected"),
        }
    }

    #[tokio::test]
    async fn empty_namespace_in_test_ctor_means_cluster_wide() {
        let engine = empty_engine_for_test();
        assert_eq!(engine.scope().await, KubeScope::AllNamespaces);
        // The wire field encodes cluster-wide as the empty string.
        assert_eq!(engine.snapshot().await.unwrap().current_namespace, "");
    }

    #[tokio::test]
    async fn scoped_engine_reports_its_namespace_on_the_wire() {
        let engine = KubeEngine::new_for_test(
            ClusterKind::Kind,
            None,
            "kube-system".into(),
            ResourceCache::default(),
            MetricsCache::default(),
        );
        assert_eq!(
            engine.scope().await,
            KubeScope::Namespace("kube-system".into())
        );
        assert_eq!(
            engine.snapshot().await.unwrap().current_namespace,
            "kube-system"
        );
    }

    #[tokio::test]
    async fn toggle_scope_flips_between_namespace_and_cluster_wide() {
        let engine = KubeEngine::new_for_test(
            ClusterKind::Kind,
            None,
            "kube-system".into(),
            ResourceCache::default(),
            MetricsCache::default(),
        );

        // Scoped -> cluster-wide.
        assert_eq!(engine.toggle_scope().await, KubeScope::AllNamespaces);
        assert_eq!(engine.snapshot().await.unwrap().current_namespace, "");

        // Cluster-wide -> back to the same namespace it started from.
        assert_eq!(
            engine.toggle_scope().await,
            KubeScope::Namespace("kube-system".into())
        );
        assert_eq!(
            engine.snapshot().await.unwrap().current_namespace,
            "kube-system"
        );
    }

    #[tokio::test]
    async fn toggle_from_cluster_wide_uses_the_configured_namespace() {
        // Built cluster-wide via the empty-string ctor: the toggle target
        // falls back to "default", mirroring a kubeconfig with no explicit
        // namespace.
        let engine = empty_engine_for_test();
        assert_eq!(
            engine.toggle_scope().await,
            KubeScope::Namespace("default".into())
        );
    }

    #[tokio::test]
    async fn rescoping_drops_out_of_scope_rows_immediately() {
        // Without this, scoping down would leave other namespaces' pods on
        // screen for up to a full poll interval — showing the user exactly
        // what they just asked to filter out.
        let resources = ResourceCache {
            pods: vec![pod_from_json(serde_json::json!({
                "metadata": {"name": "a", "namespace": "other"},
                "spec": {"nodeName": "n1"},
                "status": {"phase": "Running"}
            }))],
            deployments: vec![deployment_from_json(serde_json::json!({
                "metadata": {"name": "d", "namespace": "other"},
                "spec": {"replicas": 1},
                "status": {"readyReplicas": 1, "replicas": 1}
            }))],
            nodes: vec![node_from_json(serde_json::json!({
                "metadata": {"name": "n1"},
                "status": {"conditions": [{"type": "Ready", "status": "True"}]}
            }))],
            last_update_ms: unix_ms(),
            ..Default::default()
        };

        let engine = KubeEngine::new_for_test(
            ClusterKind::Kind,
            None,
            String::new(),
            resources,
            MetricsCache::default(),
        );
        assert_eq!(engine.snapshot().await.unwrap().pods.len(), 1);

        engine.set_scope(KubeScope::Namespace("mine".into())).await;

        let snap = engine.snapshot().await.unwrap();
        assert!(snap.pods.is_empty(), "stale pods survived the rescope");
        assert!(
            snap.deployments.is_empty(),
            "stale deployments survived the rescope"
        );
        // Nodes are cluster-scoped and unaffected by namespace scoping, so
        // they must NOT be cleared.
        assert_eq!(snap.nodes.len(), 1, "nodes must survive a rescope");
        assert_eq!(snap.current_namespace, "mine");
    }

    // ─── real cluster integration ────────────────────────────────────────
    //
    // These tests exercise the full `KubeEngine` boot path against whatever
    // cluster is at `~/.kube/config` / `$KUBECONFIG`. They are `#[ignore]`d
    // by default because muxtop's regular test suite must remain runnable
    // on machines without a kubeconfig.
    //
    // Local recipe (kind):
    //   $ kind create cluster
    //   $ cargo test -p muxtop-core --lib kube_engine -- --ignored
    //   $ kind delete cluster
    //
    // The first test waits up to 10 s after `connect()` to give the 5 s
    // resource-poll loop a chance to populate the cache; without that
    // wait the snapshot would always read `reachable = false` because
    // `last_update_ms` is still 0. We poll snapshot() in a tight loop
    // rather than `tokio::time::sleep(Duration::from_secs(10))` once so
    // the test passes as soon as the data is ready.

    /// Connect to the local cluster, wait for the resource-poll loop to
    /// publish at least one snapshot, then assert basic invariants.
    /// Requires a reachable kubeconfig context.
    #[tokio::test]
    #[ignore = "requires a reachable Kubernetes cluster (kind / k3d / EKS / etc.)"]
    async fn integration_connect_and_snapshot() {
        use crate::cluster_engine::{ClusterEngine, detect_kubeconfig};
        use std::time::{Duration, Instant};

        let source = detect_kubeconfig();
        let engine = KubeEngine::connect(source, None, None)
            .await
            .expect("connect failed — set $KUBECONFIG to a reachable cluster");

        // The engine boots with empty caches and sets reachable=true only
        // after the first poll tick (5 s by default). Give it up to 10 s.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut snap = engine.snapshot().await.expect("snapshot");
        while !snap.reachable && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            snap = engine.snapshot().await.expect("snapshot");
        }
        assert!(
            snap.reachable,
            "engine never became reachable within 10 s — is the poll loop wired?"
        );

        // Sanity invariants: a real cluster has at least one node and
        // muxtop's connect-time `/version` probe populated server_version.
        assert!(
            !snap.nodes.is_empty(),
            "expected at least one node in a real cluster"
        );
        // `server_version` is best-effort: the probe can fail without
        // failing connect(). Don't assert it's Some.
    }

    /// `--no-kube` equivalent: confirm that omitting connect() leaves the
    /// snapshot unreachable. This tests via `new_for_test` with an empty
    /// `last_update_ms = 0`, which is the same state `connect()` produces
    /// before the first poll.
    #[tokio::test]
    async fn empty_engine_snapshot_is_not_reachable() {
        let engine = KubeEngine::new_for_test(
            ClusterKind::Generic,
            None,
            String::new(),
            ResourceCache::default(),
            MetricsCache::default(),
        );
        let snap = engine.snapshot().await.unwrap();
        assert!(!snap.reachable);
    }
}

#[cfg(test)]
mod regression_tests;

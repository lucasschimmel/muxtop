//! `bollard`-backed `ContainerEngine` implementation (v0.3.0 E2).
//!
//! Handles connection (Unix socket or TCP), container listing + parallel
//! stats fetch (`buffer_unordered(16)`), error mapping (ContainerNotFound
//! filtered silently, 403 → PermissionDenied), and stop/kill/restart actions.
//!
//! CPU percentage is computed client-side by caching the previous `cpu_usage`
//! + `system_cpu_usage` per container and taking a delta at the next tick.
//!
//! The Collector calls this at 0.5 Hz (see ADR-05 of forge/23-epic-containers),
//! so the first tick after startup yields 0 % for every container — an
//! acceptable 2 s warm-up.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use bollard::Docker;
use bollard::errors::Error as BollardError;
use bollard::models::{ContainerStatsResponse, ContainerSummary, ContainerSummaryStateEnum};
use bollard::query_parameters::{
    KillContainerOptionsBuilder, ListContainersOptionsBuilder, RestartContainerOptionsBuilder,
    StatsOptionsBuilder, StopContainerOptionsBuilder,
};
use futures::stream::StreamExt;
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::container_engine::{ConnectionTarget, ContainerEngine, EngineError};
use crate::containers::{ContainerSnapshot, ContainerState, EngineKind};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const STATS_TIMEOUT: Duration = Duration::from_millis(1_500);
const ACTION_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_TIMEOUT_SECS: u64 = 120;
const PARALLEL_STATS: usize = 16;

/// Last CPU reading for a container — used to compute percentage deltas.
#[derive(Clone, Copy, Debug)]
struct CachedCpu {
    cpu_usage: u64,
    system_usage: u64,
}

/// `bollard`-backed `ContainerEngine` implementation.
pub struct DockerEngine {
    docker: Docker,
    kind: EngineKind,
    last_cpu: Mutex<HashMap<String, CachedCpu>>,
}

impl DockerEngine {
    /// Connect to a Docker/Podman daemon and probe its kind.
    ///
    /// Auto-detected paths refuse to follow symlinks. Use [`Self::connect_explicit`]
    /// when the user supplied the path themselves and may legitimately be
    /// pointing at a symlink (e.g. `/var/run/docker.sock` → `/run/docker.sock`).
    ///
    /// Bubbles `EngineError::ConnectFailed` if the connection can't be
    /// established, `EngineError::Timeout` if `/info` takes longer than 5 s.
    pub async fn connect(target: ConnectionTarget) -> Result<Self, EngineError> {
        Self::connect_with(target, false).await
    }

    /// Same as [`Self::connect`] but tolerates symlinked Unix sockets — used
    /// when the user supplied the path explicitly via `--docker-socket`.
    pub async fn connect_explicit(target: ConnectionTarget) -> Result<Self, EngineError> {
        Self::connect_with(target, true).await
    }

    async fn connect_with(
        target: ConnectionTarget,
        allow_symlink: bool,
    ) -> Result<Self, EngineError> {
        let docker = match target {
            ConnectionTarget::Unix(path) => {
                check_socket_symlink(&path, allow_symlink)?;
                let socket_path = path.to_str().ok_or_else(|| {
                    EngineError::ConnectFailed(format!("non-UTF-8 socket path: {path:?}"))
                })?;
                Docker::connect_with_socket(
                    socket_path,
                    SOCKET_TIMEOUT_SECS,
                    bollard::API_DEFAULT_VERSION,
                )
                .map_err(|e| EngineError::ConnectFailed(e.to_string()))?
            }
            ConnectionTarget::Http(url) => {
                Docker::connect_with_http(&url, SOCKET_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
                    .map_err(|e| EngineError::ConnectFailed(e.to_string()))?
            }
        };

        let info = timeout(CONNECT_TIMEOUT, docker.info())
            .await
            .map_err(|_| EngineError::Timeout(CONNECT_TIMEOUT))?
            .map_err(map_bollard_error)?;

        let kind = detect_engine_kind(
            info.server_version.as_deref(),
            info.operating_system.as_deref(),
        );

        Ok(Self {
            docker,
            kind,
            last_cpu: Mutex::new(HashMap::new()),
        })
    }
}

/// Best-effort engine construction shared by the `muxtop` and `muxtop-server`
/// binaries.
///
/// 1. If `override_socket` is `Some(path)`, treat it as a Unix socket directly.
/// 2. Otherwise, run [`crate::container_engine::detect_socket`] (DOCKER_HOST →
///    `/var/run/docker.sock` → Podman paths).
/// 3. Try [`DockerEngine::connect`]. Failure is logged as a warning and
///    swallowed — the caller gets `None` and renders the "no engine" / "no
///    daemon" fallback in the UI rather than refusing to launch.
///
/// Never returns `Err`: callers want to keep booting even when no daemon is
/// reachable.
pub async fn maybe_connect_default_engine(
    override_socket: Option<&std::path::Path>,
) -> Option<std::sync::Arc<dyn ContainerEngine + Send + Sync>> {
    use crate::container_engine::{ConnectionTarget, detect_socket};

    let explicit = override_socket.is_some();
    let target = match override_socket {
        Some(path) => ConnectionTarget::Unix(path.to_path_buf()),
        None => match detect_socket() {
            Some(t) => t,
            None => {
                tracing::debug!("no container engine socket detected (Docker/Podman)");
                return None;
            }
        },
    };

    // `connect` and `connect_explicit` are distinct opaque futures; await
    // each in its own arm so the resulting `Result` types unify.
    let result = if explicit {
        DockerEngine::connect_explicit(target.clone()).await
    } else {
        DockerEngine::connect(target.clone()).await
    };
    match result {
        Ok(engine) => {
            tracing::info!(engine = ?engine.kind(), ?target, "container engine connected");
            Some(std::sync::Arc::new(engine))
        }
        Err(err) => {
            tracing::warn!(error = %err, ?target, "container engine unreachable; Containers tab disabled");
            None
        }
    }
}

#[async_trait]
impl ContainerEngine for DockerEngine {
    async fn list_and_stats(&self) -> Result<Vec<ContainerSnapshot>, EngineError> {
        let list_opts = ListContainersOptionsBuilder::default().all(true).build();
        let containers = self
            .docker
            .list_containers(Some(list_opts))
            .await
            .map_err(map_bollard_error)?;

        // Snapshot the last-cpu cache before spawning parallel tasks.
        let last_cpu_snapshot: HashMap<String, CachedCpu> = {
            let guard = self.last_cpu.lock().await;
            guard.clone()
        };

        // Fan out stats fetches with bounded parallelism.
        let docker = self.docker.clone();
        let results: Vec<Result<(ContainerSnapshot, CachedCpu), EngineError>> =
            futures::stream::iter(containers)
                .map(|container| {
                    let docker = docker.clone();
                    let prev = container
                        .id
                        .as_deref()
                        .and_then(|id| last_cpu_snapshot.get(id).copied());
                    async move {
                        let id = container
                            .id
                            .clone()
                            .ok_or_else(|| EngineError::Other("container has no id".into()))?;
                        let (stats, started_at_ms) = tokio::join!(
                            fetch_stats(&docker, &id),
                            fetch_started_at(&docker, &id),
                        );
                        let (mut snapshot, cpu) = build_snapshot(container, &stats?, prev);
                        snapshot.started_at_ms = started_at_ms;
                        Ok((snapshot, cpu))
                    }
                })
                .buffer_unordered(PARALLEL_STATS)
                .collect()
                .await;

        let mut snapshots = Vec::with_capacity(results.len());
        let mut new_cache: HashMap<String, CachedCpu> = HashMap::new();

        for result in results {
            match result {
                Ok((snap, cached)) => {
                    new_cache.insert(snap.id_full.clone(), cached);
                    snapshots.push(snap);
                }
                // Raced with removal: drop silently.
                Err(EngineError::ContainerNotFound(_)) => {}
                // Per-container failure isolation (LOW-S4): a single 403
                // / timeout / unexpected error must not blank the whole tab.
                // Drop just this row and warn; other containers continue.
                Err(e) => {
                    tracing::warn!(error = %e, "container stats failed; dropping row");
                }
            }
        }

        *self.last_cpu.lock().await = new_cache;
        Ok(snapshots)
    }

    async fn stop(&self, id: &str, timeout_secs: Option<u64>) -> Result<(), EngineError> {
        let mut builder = StopContainerOptionsBuilder::default();
        if let Some(t) = timeout_secs {
            builder = builder.t(i32::try_from(t).unwrap_or(i32::MAX));
        }
        let opts = builder.build();
        timeout(ACTION_TIMEOUT, self.docker.stop_container(id, Some(opts)))
            .await
            .map_err(|_| EngineError::Timeout(ACTION_TIMEOUT))?
            .map_err(map_bollard_error)
    }

    async fn kill(&self, id: &str) -> Result<(), EngineError> {
        let opts = KillContainerOptionsBuilder::default()
            .signal("KILL")
            .build();
        timeout(ACTION_TIMEOUT, self.docker.kill_container(id, Some(opts)))
            .await
            .map_err(|_| EngineError::Timeout(ACTION_TIMEOUT))?
            .map_err(map_bollard_error)
    }

    async fn restart(&self, id: &str) -> Result<(), EngineError> {
        let opts = RestartContainerOptionsBuilder::default().build();
        timeout(
            ACTION_TIMEOUT,
            self.docker.restart_container(id, Some(opts)),
        )
        .await
        .map_err(|_| EngineError::Timeout(ACTION_TIMEOUT))?
        .map_err(map_bollard_error)
    }

    fn kind(&self) -> EngineKind {
        self.kind
    }
}

// ─── helpers (pure, directly unit-tested) ──────────────────────────────────

/// Reject Unix socket paths that are symlinks (LOW-S5).
///
/// Auto-detected sockets must be real sockets — a symlink at one of the
/// canonical paths is a strong sign of redirection (intentional or otherwise).
/// When `allow_symlink` is `true` (user-supplied `--docker-socket`), we log a
/// warning but proceed.
///
/// `symlink_metadata` is used so we never traverse the link itself.
fn check_socket_symlink(path: &std::path::Path, allow_symlink: bool) -> Result<(), EngineError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        // Path doesn't exist yet, or we can't stat it: bollard will surface a
        // clearer error on connect — don't second-guess here.
        Err(_) => return Ok(()),
    };

    if meta.file_type().is_symlink() {
        if allow_symlink {
            tracing::warn!(
                socket = %path.display(),
                "user-supplied docker socket is a symlink; proceeding because --docker-socket was explicit",
            );
            Ok(())
        } else {
            Err(EngineError::ConnectFailed(format!(
                "refusing to follow symlinked socket: {} (auto-detected; pass --docker-socket explicitly to override)",
                path.display()
            )))
        }
    } else {
        Ok(())
    }
}

/// Map a bollard error into our semantic [`EngineError`] enum.
fn map_bollard_error(e: BollardError) -> EngineError {
    match &e {
        BollardError::DockerResponseServerError {
            status_code,
            message,
        } => match *status_code {
            404 => EngineError::ContainerNotFound(message.clone()),
            403 => EngineError::PermissionDenied(message.clone()),
            _ => EngineError::Other(format!("HTTP {status_code}: {message}")),
        },
        BollardError::RequestTimeoutError => EngineError::Timeout(ACTION_TIMEOUT),
        BollardError::IOError { .. }
        | BollardError::HyperResponseError { .. }
        | BollardError::HttpClientError { .. }
        | BollardError::SocketNotFoundError(_) => EngineError::ConnectFailed(e.to_string()),
        _ => EngineError::Other(e.to_string()),
    }
}

/// Heuristic engine-kind detection from `/info` response.
///
/// Podman reports "podman" in `server_version` or `operating_system` fields.
fn detect_engine_kind(server_version: Option<&str>, operating_system: Option<&str>) -> EngineKind {
    let sv = server_version.unwrap_or("").to_ascii_lowercase();
    let os = operating_system.unwrap_or("").to_ascii_lowercase();
    if sv.contains("podman") || os.contains("podman") {
        EngineKind::Podman
    } else if !sv.is_empty() {
        EngineKind::Docker
    } else {
        EngineKind::Unknown
    }
}

/// Docker's canonical CPU-percentage formula.
///
/// `(cpu_delta / system_delta) * online_cpus * 100`, clamped to `[0, 100 × cores]`.
///
/// Returns `0.0` if any of the required fields is missing or if there is no
/// previous sample to diff against.
fn compute_cpu_pct(stats: &ContainerStatsResponse, prev: Option<CachedCpu>) -> f32 {
    let cpu_stats = match stats.cpu_stats.as_ref() {
        Some(s) => s,
        None => return 0.0,
    };

    let total_usage = cpu_stats
        .cpu_usage
        .as_ref()
        .and_then(|u| u.total_usage)
        .unwrap_or(0);
    let system_usage = cpu_stats.system_cpu_usage.unwrap_or(0);
    let online_cpus = cpu_stats.online_cpus.unwrap_or(1).max(1) as f64;

    let prev = match prev {
        Some(p) => p,
        None => return 0.0,
    };

    let cpu_delta = total_usage.saturating_sub(prev.cpu_usage) as f64;
    let system_delta = system_usage.saturating_sub(prev.system_usage) as f64;

    if system_delta > 0.0 {
        let pct = (cpu_delta / system_delta) * online_cpus * 100.0;
        pct.clamp(0.0, online_cpus * 100.0) as f32
    } else {
        0.0
    }
}

/// Map a bollard container-state enum into our serializable [`ContainerState`].
fn map_state(state: Option<ContainerSummaryStateEnum>) -> ContainerState {
    match state {
        Some(ContainerSummaryStateEnum::CREATED) => ContainerState::Created,
        Some(ContainerSummaryStateEnum::RUNNING) => ContainerState::Running,
        Some(ContainerSummaryStateEnum::PAUSED) => ContainerState::Paused,
        Some(ContainerSummaryStateEnum::RESTARTING) => ContainerState::Restarting,
        Some(ContainerSummaryStateEnum::EXITED) => ContainerState::Exited,
        Some(ContainerSummaryStateEnum::DEAD) => ContainerState::Dead,
        Some(ContainerSummaryStateEnum::REMOVING) => ContainerState::Removing,
        // Bollard's enum includes `EMPTY` as a "not reported" placeholder.
        _ => ContainerState::Exited,
    }
}

/// Trim the leading `/` that Docker injects in container names, and default
/// to the short id when no name is reported.
fn canonical_name(names: Option<&[String]>, id: &str) -> String {
    let raw = names.and_then(|v| v.first()).cloned();
    match raw {
        Some(s) => s.trim_start_matches('/').to_string(),
        None => id.chars().take(12).collect(),
    }
}

/// Sum `io_service_bytes_recursive` entries filtered by `op` field
/// (`"read"` / `"write"`). Safe on cgroups v2 where the recursive list is
/// empty (`0` returned).
fn sum_blkio(stats: &ContainerStatsResponse, op_label: &str) -> u64 {
    stats
        .blkio_stats
        .as_ref()
        .and_then(|b| b.io_service_bytes_recursive.as_ref())
        .map(|entries| {
            entries
                .iter()
                .filter(|e| {
                    e.op.as_deref().map(str::to_ascii_lowercase).as_deref() == Some(op_label)
                })
                .filter_map(|e| e.value)
                .fold(0u64, |acc, v| acc.saturating_add(v))
        })
        .unwrap_or(0)
}

/// Sum per-interface RX/TX byte counters from the `networks` map.
fn sum_networks(stats: &ContainerStatsResponse) -> (u64, u64) {
    stats
        .networks
        .as_ref()
        .map(|nets| {
            nets.values().fold((0u64, 0u64), |(rx, tx), n| {
                (
                    rx.saturating_add(n.rx_bytes.unwrap_or(0)),
                    tx.saturating_add(n.tx_bytes.unwrap_or(0)),
                )
            })
        })
        .unwrap_or((0, 0))
}

/// Assemble a [`ContainerSnapshot`] from a bollard summary + stats response.
///
/// Returns the snapshot plus the CPU cache entry for the next tick.
fn build_snapshot(
    summary: ContainerSummary,
    stats: &ContainerStatsResponse,
    prev: Option<CachedCpu>,
) -> (ContainerSnapshot, CachedCpu) {
    let id = summary.id.clone().unwrap_or_default();
    let short_id: String = id.chars().take(12).collect();
    let name = canonical_name(summary.names.as_deref(), &short_id);
    let image = summary.image.unwrap_or_else(|| "<unknown>".into());
    let state = map_state(summary.state);
    let status_text = summary.status.unwrap_or_default();

    let mem_used_bytes = stats
        .memory_stats
        .as_ref()
        .and_then(|m| m.usage)
        .unwrap_or(0);
    let mem_limit_bytes = stats
        .memory_stats
        .as_ref()
        .and_then(|m| m.limit)
        .unwrap_or(0);

    let (net_rx_bytes, net_tx_bytes) = sum_networks(stats);
    let block_read_bytes = sum_blkio(stats, "read");
    let block_write_bytes = sum_blkio(stats, "write");

    let cpu_pct = compute_cpu_pct(stats, prev);
    let cpu_cache = CachedCpu {
        cpu_usage: stats
            .cpu_stats
            .as_ref()
            .and_then(|c| c.cpu_usage.as_ref())
            .and_then(|u| u.total_usage)
            .unwrap_or(0),
        system_usage: stats
            .cpu_stats
            .as_ref()
            .and_then(|c| c.system_cpu_usage)
            .unwrap_or(0),
    };

    // The summary only contains creation time. A real start time is filled
    // by the inspect request; zero honestly represents unavailable uptime.
    let started_at_ms = 0;

    let snap = ContainerSnapshot {
        id: short_id,
        id_full: id,
        name,
        image,
        state,
        status_text,
        cpu_pct,
        mem_used_bytes,
        mem_limit_bytes,
        net_rx_bytes,
        net_tx_bytes,
        block_read_bytes,
        block_write_bytes,
        started_at_ms,
    };

    (snap, cpu_cache)
}

/// Inspect each collection so a restart refreshes uptime immediately. Inspect
/// failures must not discard otherwise usable stats (restricted Docker proxies).
async fn fetch_started_at(docker: &Docker, id: &str) -> u64 {
    let inspect = match timeout(STATS_TIMEOUT, docker.inspect_container(id, None)).await {
        Ok(Ok(inspect)) => inspect,
        Ok(Err(error)) => {
            tracing::debug!(%id, %error, "container start time unavailable");
            return 0;
        }
        Err(_) => {
            tracing::debug!(%id, "container start time timed out");
            return 0;
        }
    };
    inspect
        .state
        .and_then(|state| state.started_at)
        .and_then(|start| start.parse::<k8s_openapi::jiff::Timestamp>().ok())
        .and_then(|start| u64::try_from(start.as_millisecond()).ok())
        .unwrap_or(0)
}

/// One-shot stats fetch: `stream=false`, bounded by [`STATS_TIMEOUT`].
async fn fetch_stats(docker: &Docker, id: &str) -> Result<ContainerStatsResponse, EngineError> {
    let opts = StatsOptionsBuilder::default().stream(false).build();
    let mut stream = docker.stats(id, Some(opts));

    let first = timeout(STATS_TIMEOUT, stream.next())
        .await
        .map_err(|_| EngineError::Timeout(STATS_TIMEOUT))?;

    match first {
        Some(Ok(stats)) => Ok(stats),
        Some(Err(e)) => Err(map_bollard_error(e)),
        None => Err(EngineError::Other("stats stream ended immediately".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{
        ContainerBlkioStatEntry, ContainerBlkioStats, ContainerCpuStats, ContainerCpuUsage,
        ContainerMemoryStats, ContainerNetworkStats,
    };
    use std::collections::HashMap;

    // ─── detect_engine_kind ────────────────────────────────────────────────

    #[test]
    fn detect_engine_kind_finds_podman_in_server_version() {
        assert_eq!(
            detect_engine_kind(Some("4.8.3-podman"), None),
            EngineKind::Podman
        );
    }

    #[test]
    fn detect_engine_kind_finds_podman_in_os_field() {
        assert_eq!(
            detect_engine_kind(Some("4.8.3"), Some("fedora-podman")),
            EngineKind::Podman
        );
    }

    #[test]
    fn detect_engine_kind_defaults_to_docker() {
        assert_eq!(
            detect_engine_kind(Some("25.0.3"), Some("Docker Desktop 4.29.0")),
            EngineKind::Docker
        );
    }

    #[test]
    fn detect_engine_kind_unknown_when_empty() {
        assert_eq!(detect_engine_kind(None, None), EngineKind::Unknown);
        assert_eq!(detect_engine_kind(Some(""), Some("")), EngineKind::Unknown);
    }

    // ─── map_bollard_error ────────────────────────────────────────────────

    #[test]
    fn map_404_to_container_not_found() {
        let e = BollardError::DockerResponseServerError {
            status_code: 404,
            message: "No such container: abc".into(),
        };
        assert!(matches!(
            map_bollard_error(e),
            EngineError::ContainerNotFound(_)
        ));
    }

    #[test]
    fn map_403_to_permission_denied() {
        let e = BollardError::DockerResponseServerError {
            status_code: 403,
            message: "Permission denied".into(),
        };
        assert!(matches!(
            map_bollard_error(e),
            EngineError::PermissionDenied(_)
        ));
    }

    #[test]
    fn map_500_to_other_with_context() {
        let e = BollardError::DockerResponseServerError {
            status_code: 500,
            message: "internal error".into(),
        };
        let mapped = map_bollard_error(e);
        match mapped {
            EngineError::Other(m) => assert!(m.contains("500") && m.contains("internal error")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn map_request_timeout_error() {
        let mapped = map_bollard_error(BollardError::RequestTimeoutError);
        assert!(matches!(mapped, EngineError::Timeout(_)));
    }

    // ─── compute_cpu_pct ───────────────────────────────────────────────────

    fn stats_with_cpu(total: u64, system: u64, online: u32) -> ContainerStatsResponse {
        ContainerStatsResponse {
            cpu_stats: Some(ContainerCpuStats {
                cpu_usage: Some(ContainerCpuUsage {
                    total_usage: Some(total),
                    ..Default::default()
                }),
                system_cpu_usage: Some(system),
                online_cpus: Some(online),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn compute_cpu_pct_zero_without_prev() {
        let stats = stats_with_cpu(1_000, 10_000, 4);
        assert_eq!(compute_cpu_pct(&stats, None), 0.0);
    }

    #[test]
    fn compute_cpu_pct_single_core_fully_loaded() {
        let stats = stats_with_cpu(1_000, 10_000, 1);
        let prev = CachedCpu {
            cpu_usage: 0,
            system_usage: 0,
        };
        // cpu_delta=1000, system_delta=10_000, cpus=1 → 10 %
        let pct = compute_cpu_pct(&stats, Some(prev));
        assert!((pct - 10.0).abs() < 0.001, "got {pct}");
    }

    #[test]
    fn compute_cpu_pct_multi_core_scales_by_cpus() {
        let stats = stats_with_cpu(2_000, 10_000, 4);
        let prev = CachedCpu {
            cpu_usage: 0,
            system_usage: 0,
        };
        // 2000/10000 * 4 * 100 = 80 %
        let pct = compute_cpu_pct(&stats, Some(prev));
        assert!((pct - 80.0).abs() < 0.001, "got {pct}");
    }

    #[test]
    fn compute_cpu_pct_handles_zero_system_delta() {
        let stats = stats_with_cpu(5_000, 1_000, 4);
        let prev = CachedCpu {
            cpu_usage: 0,
            system_usage: 1_000, // same → zero delta
        };
        assert_eq!(compute_cpu_pct(&stats, Some(prev)), 0.0);
    }

    #[test]
    fn compute_cpu_pct_saturating_on_counter_reset() {
        // Container restart → new total < prev total. Saturating sub prevents underflow.
        let stats = stats_with_cpu(500, 10_000, 2);
        let prev = CachedCpu {
            cpu_usage: 1_000, // higher
            system_usage: 5_000,
        };
        let pct = compute_cpu_pct(&stats, Some(prev));
        // cpu_delta=0, system_delta=5000 → 0 %
        assert_eq!(pct, 0.0);
    }

    // ─── map_state ─────────────────────────────────────────────────────────

    #[test]
    fn map_state_covers_all_variants() {
        assert_eq!(
            map_state(Some(ContainerSummaryStateEnum::CREATED)),
            ContainerState::Created
        );
        assert_eq!(
            map_state(Some(ContainerSummaryStateEnum::RUNNING)),
            ContainerState::Running
        );
        assert_eq!(
            map_state(Some(ContainerSummaryStateEnum::PAUSED)),
            ContainerState::Paused
        );
        assert_eq!(
            map_state(Some(ContainerSummaryStateEnum::RESTARTING)),
            ContainerState::Restarting
        );
        assert_eq!(
            map_state(Some(ContainerSummaryStateEnum::EXITED)),
            ContainerState::Exited
        );
        assert_eq!(
            map_state(Some(ContainerSummaryStateEnum::DEAD)),
            ContainerState::Dead
        );
        assert_eq!(
            map_state(Some(ContainerSummaryStateEnum::REMOVING)),
            ContainerState::Removing
        );
        assert_eq!(map_state(None), ContainerState::Exited);
    }

    // ─── canonical_name ────────────────────────────────────────────────────

    #[test]
    fn canonical_name_strips_leading_slash() {
        let names = vec!["/nginx".to_string()];
        assert_eq!(canonical_name(Some(&names), "abc"), "nginx");
    }

    #[test]
    fn canonical_name_falls_back_to_short_id() {
        assert_eq!(canonical_name(None, "abc123def4567890"), "abc123def456");
    }

    // ─── sum_blkio / sum_networks ─────────────────────────────────────────

    #[test]
    fn sum_blkio_reads_and_writes() {
        let stats = ContainerStatsResponse {
            blkio_stats: Some(ContainerBlkioStats {
                io_service_bytes_recursive: Some(vec![
                    ContainerBlkioStatEntry {
                        op: Some("Read".into()),
                        value: Some(1_000),
                        ..Default::default()
                    },
                    ContainerBlkioStatEntry {
                        op: Some("write".into()),
                        value: Some(500),
                        ..Default::default()
                    },
                    ContainerBlkioStatEntry {
                        op: Some("read".into()),
                        value: Some(250),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(sum_blkio(&stats, "read"), 1_250);
        assert_eq!(sum_blkio(&stats, "write"), 500);
    }

    #[test]
    fn sum_blkio_zero_when_absent() {
        let stats = ContainerStatsResponse::default();
        assert_eq!(sum_blkio(&stats, "read"), 0);
    }

    #[test]
    fn sum_networks_aggregates_all_interfaces() {
        let mut nets = HashMap::new();
        nets.insert(
            "eth0".into(),
            ContainerNetworkStats {
                rx_bytes: Some(100),
                tx_bytes: Some(50),
                ..Default::default()
            },
        );
        nets.insert(
            "eth1".into(),
            ContainerNetworkStats {
                rx_bytes: Some(200),
                tx_bytes: Some(75),
                ..Default::default()
            },
        );
        let stats = ContainerStatsResponse {
            networks: Some(nets),
            ..Default::default()
        };
        assert_eq!(sum_networks(&stats), (300, 125));
    }

    #[test]
    fn sum_networks_zero_when_absent() {
        let stats = ContainerStatsResponse::default();
        assert_eq!(sum_networks(&stats), (0, 0));
    }

    // ─── build_snapshot ────────────────────────────────────────────────────

    #[test]
    fn build_snapshot_wires_all_fields() {
        let summary = ContainerSummary {
            id: Some("abcdef1234567890".into()),
            names: Some(vec!["/my-nginx".into()]),
            image: Some("nginx:1.27".into()),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            status: Some("Up 5 minutes".into()),
            created: Some(1_700_000_000),
            ..Default::default()
        };

        let mut nets = HashMap::new();
        nets.insert(
            "eth0".into(),
            ContainerNetworkStats {
                rx_bytes: Some(1_000_000),
                tx_bytes: Some(500_000),
                ..Default::default()
            },
        );
        let stats = ContainerStatsResponse {
            memory_stats: Some(ContainerMemoryStats {
                usage: Some(128 * 1024 * 1024),
                limit: Some(512 * 1024 * 1024),
                ..Default::default()
            }),
            cpu_stats: Some(ContainerCpuStats {
                cpu_usage: Some(ContainerCpuUsage {
                    total_usage: Some(2_000),
                    ..Default::default()
                }),
                system_cpu_usage: Some(10_000),
                online_cpus: Some(2),
                ..Default::default()
            }),
            networks: Some(nets),
            blkio_stats: Some(ContainerBlkioStats {
                io_service_bytes_recursive: Some(vec![ContainerBlkioStatEntry {
                    op: Some("read".into()),
                    value: Some(1024),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let prev = CachedCpu {
            cpu_usage: 0,
            system_usage: 0,
        };
        let (snap, cache) = build_snapshot(summary, &stats, Some(prev));

        assert_eq!(snap.id, "abcdef123456");
        assert_eq!(snap.id_full, "abcdef1234567890");
        assert_eq!(snap.name, "my-nginx");
        assert_eq!(snap.image, "nginx:1.27");
        assert_eq!(snap.state, ContainerState::Running);
        assert_eq!(snap.status_text, "Up 5 minutes");
        assert!(
            (snap.cpu_pct - 40.0).abs() < 0.01,
            "cpu_pct was {}",
            snap.cpu_pct
        );
        assert_eq!(snap.mem_used_bytes, 128 * 1024 * 1024);
        assert_eq!(snap.mem_limit_bytes, 512 * 1024 * 1024);
        assert_eq!(snap.net_rx_bytes, 1_000_000);
        assert_eq!(snap.net_tx_bytes, 500_000);
        assert_eq!(snap.block_read_bytes, 1024);
        assert_eq!(snap.block_write_bytes, 0);
        assert_eq!(snap.started_at_ms, 0);
        assert_eq!(cache.cpu_usage, 2_000);
        assert_eq!(cache.system_usage, 10_000);
    }

    // ─── check_socket_symlink (LOW-S5) ────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn symlink_socket_is_rejected_for_autodetect() {
        use std::os::unix::fs as unix_fs;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.sock");
        std::fs::File::create(&real).unwrap();
        let link = dir.path().join("link.sock");
        unix_fs::symlink(&real, &link).unwrap();

        let err = check_socket_symlink(&link, /* allow_symlink */ false)
            .expect_err("symlink should be rejected");
        match err {
            EngineError::ConnectFailed(msg) => assert!(
                msg.contains("symlink"),
                "expected symlink mention, got {msg:?}"
            ),
            other => panic!("expected ConnectFailed, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_socket_is_allowed_when_explicit() {
        use std::os::unix::fs as unix_fs;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.sock");
        std::fs::File::create(&real).unwrap();
        let link = dir.path().join("link.sock");
        unix_fs::symlink(&real, &link).unwrap();

        // allow_symlink=true → ok (warns but proceeds).
        check_socket_symlink(&link, /* allow_symlink */ true).expect("explicit should allow");
    }

    #[test]
    fn non_symlink_socket_passes() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.sock");
        std::fs::File::create(&real).unwrap();
        check_socket_symlink(&real, false).expect("plain file should pass");
    }

    #[test]
    fn missing_socket_passes_through() {
        // The `connect` call will fail later with a clearer message — we don't
        // pre-empt that here.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-here.sock");
        check_socket_symlink(&missing, false).expect("missing path should pass");
    }

    // ─── per-container error isolation (LOW-S4) ───────────────────────────

    /// Replicates the result-handling loop of `list_and_stats` to verify a
    /// single per-container error does NOT abort the whole batch.
    #[test]
    fn per_container_error_is_isolated() {
        // Synthetic results: one ok, one PermissionDenied, one ContainerNotFound.
        let ok_snap = ContainerSnapshot {
            id: "aaaaaaaaaaaa".into(),
            id_full: "aaaaaaaaaaaa".to_string() + &"0".repeat(52),
            name: "ok".into(),
            image: "img:1".into(),
            state: ContainerState::Running,
            status_text: "Up".into(),
            cpu_pct: 0.0,
            mem_used_bytes: 0,
            mem_limit_bytes: 0,
            net_rx_bytes: 0,
            net_tx_bytes: 0,
            block_read_bytes: 0,
            block_write_bytes: 0,
            started_at_ms: 0,
        };
        let cached = CachedCpu {
            cpu_usage: 0,
            system_usage: 0,
        };
        let results: Vec<Result<(ContainerSnapshot, CachedCpu), EngineError>> = vec![
            Ok((ok_snap.clone(), cached)),
            Err(EngineError::PermissionDenied("denied".into())),
            Err(EngineError::ContainerNotFound("removed".into())),
        ];

        // Mirror of the loop in `list_and_stats`.
        let mut snapshots = Vec::new();
        let mut new_cache: HashMap<String, CachedCpu> = HashMap::new();
        for result in results {
            match result {
                Ok((snap, cached)) => {
                    new_cache.insert(snap.id_full.clone(), cached);
                    snapshots.push(snap);
                }
                Err(EngineError::ContainerNotFound(_)) => {}
                Err(_e) => {
                    // Drop and continue (the production path also tracing::warn!s).
                }
            }
        }

        assert_eq!(snapshots.len(), 1, "only the ok row should survive");
        assert_eq!(snapshots[0].id, "aaaaaaaaaaaa");
        assert_eq!(new_cache.len(), 1);
    }

    // ─── real Docker integration ──────────────────────────────────────────

    /// Requires a running Docker daemon. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a running Docker daemon"]
    async fn integration_connect_and_list() {
        let target = crate::container_engine::detect_socket().expect("no docker socket found");
        let engine = DockerEngine::connect(target).await.expect("connect failed");
        assert_ne!(engine.kind(), EngineKind::Unknown);
        let _snaps = engine.list_and_stats().await.expect("list_and_stats");
        // Don't assert count — test environment may have 0+ containers.
    }
}

#[cfg(test)]
mod regression_tests;
#[cfg(test)]
pub(crate) mod test_support;

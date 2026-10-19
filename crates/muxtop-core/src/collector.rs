// Async collection loop (tokio task).
//
// Four concurrent loops:
//  - System loop (configurable tick, default 1 Hz) — produces `SystemSnapshot`s
//    to the mpsc channel. Each tick reads the latest container + kube + gpu
//    snapshots (if any) and embeds them into the emitted `SystemSnapshot`.
//  - Container loop (0.5 Hz, only if an engine is provided) — calls
//    `ContainerEngine::list_and_stats()` and writes the result into a shared
//    `Arc<Mutex<Option<ContainersSnapshot>>>`. On error, publishes
//    `ContainersSnapshot::unavailable()` so the UI can render the notice.
//  - Cluster loop (0.2 Hz, only if a cluster engine is provided) — calls
//    `ClusterEngine::snapshot()` and writes the result into a shared
//    `Arc<Mutex<Option<KubeSnapshot>>>`. On error, publishes
//    `KubeSnapshot::unavailable()`. Note that the engine's own internal
//    poll task drives kube-rs LIST/metrics traffic; this collector loop
//    just samples the engine's already-cached state every 5 s.
//  - GPU loop (1 Hz, only if a GPU engine is provided) — calls
//    `GpuEngine::snapshot()` and writes the result into a shared
//    `Arc<Mutex<Option<GpusSnapshot>>>`. On error, publishes
//    `GpusSnapshot::unavailable()`.
//
// All loops share a single `CancellationToken`. Shutdown is cooperative.

use std::sync::Arc;
use std::time::Duration;

use sysinfo::{MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cluster_engine::ClusterEngine;
use crate::container_engine::ContainerEngine;
use crate::containers::ContainersSnapshot;
use crate::gpu::GpusSnapshot;
use crate::gpu_engine::GpuEngine;
use crate::kube::KubeSnapshot;
use crate::system::SystemSnapshot;

/// Container stats refresh cadence — independent from the system tick.
///
/// 0.5 Hz (2 s) keeps the HTTP cost below ~10 % of a 0.5 Hz budget at 100
/// containers (see forge/23-epic-containers ADR-05).
const CONTAINER_INTERVAL: Duration = Duration::from_secs(2);

/// Cluster snapshot sampling cadence — matches the kube-rs internal poll
/// task interval (see ADR-05 v0.4 in `forge/32-v04-kubernetes-epics/`).
const CLUSTER_INTERVAL: Duration = Duration::from_secs(5);

/// GPU snapshot cadence — deliberately the fastest of the three engine loops.
///
/// NVML computes `utilization_rates` over an internal window of roughly one
/// second. Sampling slower than that aliases the signal: a GPU pinned at
/// 100 % renders as an erratic sawtooth because each poll lands on a
/// different part of the driver's window. The cost is bounded — an NVML
/// device query is sub-millisecond and the AMD backend reads a handful of
/// small sysfs files — so matching the driver's own cadence is the cheap
/// correct choice rather than a performance trade-off.
const GPU_INTERVAL: Duration = Duration::from_secs(1);

/// Exactly the per-process fields [`SystemSnapshot::collect`] reads — and
/// nothing else (SEC-04 / PERF-01).
///
/// `ProcessRefreshKind::everything()` additionally collects `disk_usage`,
/// `environ`, `cwd`, `root` and `exe`, none of which muxtop renders. Two
/// distinct costs come with them:
///
/// * `disk_usage` is refreshed unconditionally on *every* tick, so on Linux
///   it opens and parses `/proc/<pid>/io` once per process per second.
/// * `environ` is read once and then kept in sysinfo's process table for
///   the lifetime of the process. Running muxtop as root therefore parked a
///   copy of every process's environment — API keys, tokens, database URLs —
///   in our address space, where a core dump or an attached debugger would
///   find it. Data we never display is data we should never hold.
///
/// `pid`, `parent`, `name` and `status` come from `/proc/<pid>/stat`, which
/// is parsed regardless, so they need no flag. `cmd` and `user` use
/// `OnlyIfNotSet`: both are fixed at exec time, so re-reading them every
/// tick buys nothing.
///
/// `nothing()` keeps `tasks: true` (its one non-false default), so the
/// process list still contains what it did before.
fn process_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_user(UpdateKind::OnlyIfNotSet)
}

pub struct Collector {
    sys: sysinfo::System,
    networks: sysinfo::Networks,
    interval: Duration,
    container_engine: Option<Arc<dyn ContainerEngine + Send + Sync>>,
    cluster_engine: Option<Arc<dyn ClusterEngine + Send + Sync>>,
    gpu_engine: Option<Arc<dyn GpuEngine + Send + Sync>>,
}

impl Collector {
    /// Create a collector that only gathers system snapshots.
    pub fn new(interval: Duration) -> Self {
        Self::with_all_engines(interval, None, None, None)
    }

    /// Create a collector that also polls a container engine.
    ///
    /// `container_engine = None` disables the container path entirely — the
    /// emitted `SystemSnapshot.containers` stays `None`.
    pub fn with_container_engine(
        interval: Duration,
        container_engine: Option<Arc<dyn ContainerEngine + Send + Sync>>,
    ) -> Self {
        Self::with_all_engines(interval, container_engine, None, None)
    }

    /// Create a collector with both container and cluster engines.
    ///
    /// Either side can be `None` independently. The emitted snapshot's
    /// `containers` / `kube` fields stay `None` for the disabled side.
    ///
    /// Retained for callers predating v0.5; [`Self::with_all_engines`] is the
    /// full constructor.
    pub fn with_engines(
        interval: Duration,
        container_engine: Option<Arc<dyn ContainerEngine + Send + Sync>>,
        cluster_engine: Option<Arc<dyn ClusterEngine + Send + Sync>>,
    ) -> Self {
        Self::with_all_engines(interval, container_engine, cluster_engine, None)
    }

    /// Create a collector with every optional engine attached (v0.5).
    ///
    /// Each engine is independently optional; the corresponding
    /// `SystemSnapshot` field stays `None` for a disabled side.
    pub fn with_all_engines(
        interval: Duration,
        container_engine: Option<Arc<dyn ContainerEngine + Send + Sync>>,
        cluster_engine: Option<Arc<dyn ClusterEngine + Send + Sync>>,
        gpu_engine: Option<Arc<dyn GpuEngine + Send + Sync>>,
    ) -> Self {
        Self {
            // `new_all()` would run a full `refresh_all()` here, pulling in
            // the environ/cwd/root/exe that `process_refresh_kind` skips.
            // Start empty; `run` seeds the baseline with the targeted refresh.
            sys: sysinfo::System::new(),
            networks: sysinfo::Networks::new_with_refreshed_list(),
            interval,
            container_engine,
            cluster_engine,
            gpu_engine,
        }
    }

    /// Spawn the collector as one (or two) background tokio tasks.
    ///
    /// The returned `JoinHandle` completes when the SYSTEM loop exits; the
    /// container loop (if present) is driven by the same cancellation token
    /// and joined internally before the handle completes.
    pub fn spawn(
        self,
        tx: mpsc::Sender<SystemSnapshot>,
        token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(Self::run(self, tx, token))
    }

    /// Refresh only the subsystems the TUI renders.
    ///
    /// PERF-L2 established the outer scope (`refresh_all` also rebuilds disk,
    /// component and user tables we never read); [`process_refresh_kind`]
    /// narrows the per-process side the same way.
    fn refresh_targeted(&mut self) {
        self.sys
            .refresh_memory_specifics(MemoryRefreshKind::everything());
        self.sys.refresh_cpu_usage();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh_kind());
    }

    async fn run(mut self, tx: mpsc::Sender<SystemSnapshot>, token: CancellationToken) {
        // A private child token stops sibling loops without cancelling the caller.
        let token = token.child_token();
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // First tick completes immediately — seed the sysinfo delta baseline
        // so CPU % and network counters report useful values from tick #2.
        interval.tick().await;
        self.refresh_targeted();
        self.networks.refresh(true);

        // Shared container + kube snapshot slots — written by their
        // respective polling tasks, read by the system loop on every emit.
        //
        // Semantic for the kube slot:
        //   - `None`              → engine attached, first poll has not yet
        //                           completed (transient, ≤ 5 s after boot).
        //                           UI renders "Waiting for cluster data…".
        //   - `Some(unavailable)` → engine attempt FAILED (no kubeconfig,
        //                           cluster unreachable, etc.). Stable.
        //                           UI renders "No cluster reachable…".
        //   - `Some(ok)`          → engine returning live data.
        //
        // We seed the slot with `Some(unavailable)` when no engine is
        // configured so the UI hits the actionable "no cluster" message
        // immediately instead of "Waiting…" forever (otherwise
        // indistinguishable from a slow first poll).
        // PERF-02: the slots hold `Arc`s, so the per-tick read below is a
        // refcount bump instead of a deep clone of data that changes at
        // 0.5 Hz / 0.2 Hz.
        let last_containers: Arc<Mutex<Option<Arc<ContainersSnapshot>>>> =
            Arc::new(Mutex::new(None));
        let initial_kube = if self.cluster_engine.is_some() {
            None
        } else {
            Some(Arc::new(KubeSnapshot::unavailable()))
        };
        let last_kube: Arc<Mutex<Option<Arc<KubeSnapshot>>>> = Arc::new(Mutex::new(initial_kube));

        // Same three-state semantic for the GPU slot. `--no-gpu` and the
        // remote client both arrive here with no engine, and seeding
        // `Some(unavailable)` makes the tab say "no GPU" straight away
        // instead of "Waiting…" forever.
        let initial_gpu = if self.gpu_engine.is_some() {
            None
        } else {
            Some(GpusSnapshot::unavailable())
        };
        let last_gpu: Arc<Mutex<Option<GpusSnapshot>>> = Arc::new(Mutex::new(initial_gpu));

        // Spawn the container polling loop if an engine is configured.
        let container_task = self.container_engine.take().map(|engine| {
            spawn_container_loop(engine, Arc::clone(&last_containers), token.clone())
        });
        // Spawn the cluster polling loop if a kube engine is configured.
        let cluster_task = self
            .cluster_engine
            .take()
            .map(|engine| spawn_cluster_loop(engine, Arc::clone(&last_kube), token.clone()));
        // Spawn the GPU polling loop if an engine is configured.
        let gpu_task = self
            .gpu_engine
            .take()
            .map(|engine| spawn_gpu_loop(engine, Arc::clone(&last_gpu), token.clone()));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.refresh_targeted();
                    self.networks.refresh(false);

                    let containers = last_containers.lock().await.clone();
                    let kube = last_kube.lock().await.clone();
                    let gpu = last_gpu.lock().await.clone();
                    let snapshot =
                        SystemSnapshot::collect(&self.sys, &self.networks, containers, kube, gpu);

                    match tx.try_send(snapshot) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::trace!("channel full, dropping snapshot");
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            tracing::debug!("channel closed, stopping collector");
                            break;
                        }
                    }
                }
                _ = tx.closed() => break,
                _ = token.cancelled() => {
                    tracing::debug!("collector shutting down");
                    break;
                }
            }
        }

        token.cancel();

        // Wait for the container + cluster + GPU tasks to wind down so the
        // JoinHandle reflects total shutdown.
        for (engine, task) in [
            ("container", container_task),
            ("cluster", cluster_task),
            ("gpu", gpu_task),
        ] {
            if let Some(handle) = task
                && let Err(error) = handle.await
            {
                tracing::error!(engine, %error, "collector engine task failed");
            }
        }
    }
}

/// Background loop polling the GPU engine at 1 Hz (see [`GPU_INTERVAL`]).
/// Writes into the shared slot; publishes `GpusSnapshot::unavailable()` on
/// failure so the UI renders the "no GPU" state rather than freezing on the
/// last good sample.
fn spawn_gpu_loop(
    engine: Arc<dyn GpuEngine + Send + Sync>,
    slot: Arc<Mutex<Option<GpusSnapshot>>>,
    token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(GPU_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match tokio::select! {
                        result = engine.snapshot() => result,
                        () = token.cancelled() => break,
                    } {
                        Ok(snapshot) => {
                            *slot.lock().await = Some(snapshot);
                        }
                        Err(err) => {
                            tracing::warn!(target: "muxtop::gpu", error = %err, "GPU engine failed");
                            *slot.lock().await =
                                Some(GpusSnapshot::unavailable_with(err.to_string()));
                        }
                    }
                }
                _ = token.cancelled() => {
                    tracing::debug!("gpu loop shutting down");
                    break;
                }
            }
        }
    })
}

/// Background loop polling the cluster engine at 0.2 Hz (5 s). Writes into
/// the shared slot; publishes `KubeSnapshot::unavailable()` on failure so
/// the UI can render the "no cluster" state.
///
/// Note the engine's own kube-rs poll task drives the actual LIST + metrics
/// traffic; this loop just samples the engine's cached state and forwards
/// it through the snapshot pipeline.
fn spawn_cluster_loop(
    engine: Arc<dyn ClusterEngine + Send + Sync>,
    slot: Arc<Mutex<Option<Arc<KubeSnapshot>>>>,
    token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CLUSTER_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match tokio::select! {
                        result = engine.snapshot() => result,
                        () = token.cancelled() => break,
                    } {
                        Ok(snapshot) => {
                            *slot.lock().await = Some(Arc::new(snapshot));
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "cluster engine failed");
                            *slot.lock().await = Some(Arc::new(KubeSnapshot::unavailable()));
                        }
                    }
                }
                _ = token.cancelled() => {
                    tracing::debug!("cluster loop shutting down");
                    break;
                }
            }
        }
    })
}

/// Background loop polling the container engine at 0.5 Hz. Writes into the
/// shared slot; publishes `ContainersSnapshot::unavailable()` on failure so
/// the UI can render the "no daemon" state.
fn spawn_container_loop(
    engine: Arc<dyn ContainerEngine + Send + Sync>,
    slot: Arc<Mutex<Option<Arc<ContainersSnapshot>>>>,
    token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CONTAINER_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match tokio::select! {
                        result = engine.list_and_stats() => result,
                        () = token.cancelled() => break,
                    } {
                        Ok(containers) => {
                            let snapshot = ContainersSnapshot {
                                engine: engine.kind(),
                                daemon_up: true,
                                containers,
                            };
                            *slot.lock().await = Some(Arc::new(snapshot));
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "container engine failed");
                            *slot.lock().await = Some(Arc::new(ContainersSnapshot::unavailable()));
                        }
                    }
                }
                _ = token.cancelled() => {
                    tracing::debug!("container loop shutting down");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_engine::EngineError;
    use crate::containers::{ContainerSnapshot, ContainerState, EngineKind};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    // ─── SEC-04 / PERF-01: refresh scope ──────────────────────────────────

    /// Locks the refresh scope to what `SystemSnapshot::collect` reads. The
    /// exclusions are the point: `disk_usage` costs a `/proc/<pid>/io` parse
    /// per process per tick, and `environ` parks every process's secrets in
    /// our address space for the lifetime of the run.
    #[test]
    fn process_refresh_kind_collects_only_rendered_fields() {
        let rk = process_refresh_kind();

        assert!(rk.cpu(), "cpu_percent column");
        assert!(rk.memory(), "memory_bytes / memory_percent columns");
        assert_eq!(rk.cmd(), UpdateKind::OnlyIfNotSet, "command column");
        assert_eq!(rk.user(), UpdateKind::OnlyIfNotSet, "user column");

        assert!(!rk.disk_usage(), "never rendered; costs a syscall per tick");
        assert_eq!(rk.environ(), UpdateKind::Never, "never rendered; sensitive");
        assert_eq!(rk.cwd(), UpdateKind::Never, "never rendered");
        assert_eq!(rk.root(), UpdateKind::Never, "never rendered");
        assert_eq!(rk.exe(), UpdateKind::Never, "never rendered");

        assert!(
            rk.tasks(),
            "narrowing the refresh must not silently drop threads from the list"
        );
    }

    /// The narrowed scope must still populate every column the TUI draws —
    /// a refresh kind that collected nothing would also pass the assertions
    /// above.
    #[test]
    fn targeted_refresh_still_populates_rendered_fields() {
        let mut collector = Collector::new(Duration::from_millis(10));
        collector.refresh_targeted();

        let processes = collector.sys.processes();
        assert!(!processes.is_empty(), "expected a populated process table");

        let me = sysinfo::get_current_pid().expect("current pid");
        let proc_info = processes.get(&me).expect("our own process is listed");

        assert!(
            !proc_info.name().is_empty(),
            "name feeds the NAME column and the kill confirmation"
        );
        assert!(
            !proc_info.cmd().is_empty(),
            "cmd feeds the COMMAND column and must survive OnlyIfNotSet"
        );
        assert!(
            proc_info.memory() > 0,
            "memory feeds the MEM columns: {}",
            proc_info.memory()
        );
    }

    // ─── mock engine ──────────────────────────────────────────────────────

    /// Test double for `ContainerEngine`: returns a static list and counts
    /// invocations. `fail_mode = true` makes `list_and_stats` return an
    /// error so we can exercise the degraded path.
    struct MockEngine {
        kind: EngineKind,
        call_count: AtomicUsize,
        fail_mode: AtomicBool,
        hang_mode: AtomicBool,
    }

    impl MockEngine {
        fn new(kind: EngineKind) -> Self {
            Self {
                kind,
                call_count: AtomicUsize::new(0),
                fail_mode: AtomicBool::new(false),
                hang_mode: AtomicBool::new(false),
            }
        }

        fn sample_container() -> ContainerSnapshot {
            ContainerSnapshot {
                id: "abc123".into(),
                id_full: "abc123".to_string() + &"0".repeat(58),
                name: "mock-svc".into(),
                image: "mock:latest".into(),
                state: ContainerState::Running,
                status_text: "Up 1 minute".into(),
                cpu_pct: 2.5,
                mem_used_bytes: 128 * 1024 * 1024,
                mem_limit_bytes: 512 * 1024 * 1024,
                net_rx_bytes: 1024,
                net_tx_bytes: 512,
                block_read_bytes: 0,
                block_write_bytes: 0,
                started_at_ms: 1_700_000_000_000,
            }
        }
    }

    #[async_trait]
    impl ContainerEngine for MockEngine {
        async fn list_and_stats(&self) -> Result<Vec<ContainerSnapshot>, EngineError> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            if self.hang_mode.load(Ordering::Relaxed) {
                return std::future::pending().await;
            }
            if self.fail_mode.load(Ordering::Relaxed) {
                return Err(EngineError::ConnectFailed("mock failure".into()));
            }
            Ok(vec![Self::sample_container()])
        }
        async fn stop(&self, _id: &str, _t: Option<u64>) -> Result<(), EngineError> {
            Ok(())
        }
        async fn kill(&self, _id: &str) -> Result<(), EngineError> {
            Ok(())
        }
        async fn restart(&self, _id: &str) -> Result<(), EngineError> {
            Ok(())
        }
        fn kind(&self) -> EngineKind {
            self.kind
        }
    }

    fn make_collector(
        cap: usize,
    ) -> (
        mpsc::Receiver<SystemSnapshot>,
        tokio::task::JoinHandle<()>,
        CancellationToken,
    ) {
        let (tx, rx) = mpsc::channel(cap);
        let token = CancellationToken::new();
        let collector = Collector::new(Duration::from_secs(1));
        let handle = collector.spawn(tx, token.clone());
        (rx, handle, token)
    }

    // ─── Pre-existing system-only tests ───────────────────────────────────

    // @generated by:gauntlet run:muxtop-audit-fixes
    #[tokio::test]
    async fn t_ac_04_receiver_closure_cancels_inflight_engine_poll() {
        let engine = Arc::new(MockEngine::new(EngineKind::Docker));
        engine.hang_mode.store(true, Ordering::Relaxed);
        let collector =
            Collector::with_container_engine(Duration::from_millis(10), Some(engine.clone()));
        let (tx, rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let handle = collector.spawn(tx, cancel.clone());
        tokio::time::timeout(Duration::from_secs(2), async {
            while engine.call_count.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("engine must start polling");
        drop(rx);
        let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        cancel.cancel();
        assert!(
            result.is_ok(),
            "shutdown must interrupt an engine that never returns"
        );
    }

    // @generated by:gauntlet run:muxtop-audit-fixes
    #[tokio::test]
    async fn t_ac_04_collector_stops_engines_when_receiver_closes() {
        let engine = Arc::new(MockEngine::new(EngineKind::Docker));
        let collector = Collector::with_container_engine(Duration::from_millis(10), Some(engine));
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let parent = CancellationToken::new();
        let handle = collector.spawn(tx, parent.clone());
        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        parent.cancel();
        assert!(
            result.is_ok(),
            "collector must stop all engine loops on channel closure"
        );
    }

    #[tokio::test]
    async fn test_collector_produces_snapshots() {
        let (mut rx, handle, token) = make_collector(4);

        let mut count = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);

        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(_)) => {
                    count += 1;
                    if count >= 2 {
                        break;
                    }
                }
                Ok(None) => panic!("channel closed before receiving 2 snapshots"),
                Err(_) => panic!("timeout: only received {count} snapshots within 4s"),
            }
        }

        token.cancel();
        handle.await.expect("collector task panicked");
        assert!(count >= 2, "expected at least 2 snapshots, got {count}");
    }

    #[tokio::test]
    async fn test_collector_snapshot_has_data() {
        let (mut rx, handle, token) = make_collector(4);

        let snapshot = tokio::time::timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("timeout waiting for snapshot")
            .expect("channel closed before first snapshot");

        token.cancel();
        handle.await.expect("collector task panicked");

        assert!(
            !snapshot.processes.is_empty(),
            "snapshot should contain processes"
        );
        assert!(
            !snapshot.cpu.cores.is_empty(),
            "snapshot should contain CPU cores"
        );
        // With no engine, containers stays None.
        assert!(snapshot.containers.is_none());
    }

    #[tokio::test]
    async fn test_collector_graceful_shutdown() {
        let (mut rx, handle, token) = make_collector(4);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        tokio::time::sleep(Duration::from_millis(500)).await;
        token.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("collector did not shut down within 2s")
            .expect("collector task panicked");
    }

    #[tokio::test]
    async fn test_collector_channel_backpressure() {
        let (tx, _rx) = mpsc::channel::<SystemSnapshot>(1);
        let token = CancellationToken::new();
        let collector = Collector::new(Duration::from_secs(1));
        let handle = collector.spawn(tx, token.clone());

        tokio::time::sleep(Duration::from_secs(2)).await;
        token.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("collector did not shut down within 2s after backpressure test")
            .expect("collector task panicked");
    }

    #[tokio::test]
    async fn test_collector_respects_interval() {
        let (mut rx, handle, token) = make_collector(4);

        let first = tokio::time::timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("timeout waiting for first snapshot")
            .expect("channel closed before first snapshot");

        let second = tokio::time::timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("timeout waiting for second snapshot")
            .expect("channel closed before second snapshot");

        token.cancel();
        handle.await.expect("collector task panicked");

        let gap_ms = second.timestamp_ms.saturating_sub(first.timestamp_ms);
        assert!(
            (500..=1500).contains(&gap_ms),
            "expected gap ~1000ms, got {gap_ms}ms"
        );
    }

    // ─── Container integration tests (E4) ─────────────────────────────────

    /// With a MockEngine injected, after ~2 s the containers field should be
    /// populated with a successful snapshot.
    #[tokio::test]
    async fn test_collector_populates_containers_with_engine() {
        let engine: Arc<dyn ContainerEngine + Send + Sync> =
            Arc::new(MockEngine::new(EngineKind::Docker));
        let (tx, mut rx) = mpsc::channel(8);
        let token = CancellationToken::new();
        let collector = Collector::with_container_engine(Duration::from_millis(300), Some(engine));
        let handle = collector.spawn(tx, token.clone());

        // Drain snapshots for up to 5 s, looking for one where `containers` is
        // populated with a running container. The container task fires on a
        // 2 s cadence, so the first populated snapshot arrives around the 2 s
        // mark.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut populated: Option<SystemSnapshot> = None;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(snap)) => {
                    if snap.containers.is_some() {
                        populated = Some(snap);
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        token.cancel();
        // Don't strictly require the handle to join within the cancel deadline
        // — the container loop may have been mid-tick — but it should have
        // finished by the time the test teardown runs.
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;

        let snap = populated.expect("no snapshot with containers populated within 5 s");
        let cs = snap.containers.unwrap();
        assert!(cs.daemon_up);
        assert_eq!(cs.engine, EngineKind::Docker);
        assert_eq!(cs.containers.len(), 1);
        assert_eq!(cs.containers[0].name, "mock-svc");
    }

    /// When the engine fails, the container snapshot should be the
    /// `unavailable()` sentinel, not an absence — so the UI knows to render
    /// the "no daemon" state explicitly.
    #[tokio::test]
    async fn test_collector_publishes_unavailable_on_engine_error() {
        let engine = Arc::new(MockEngine::new(EngineKind::Docker));
        engine.fail_mode.store(true, Ordering::Relaxed);
        let engine_dyn: Arc<dyn ContainerEngine + Send + Sync> = engine.clone();
        let (tx, mut rx) = mpsc::channel(8);
        let token = CancellationToken::new();
        let collector =
            Collector::with_container_engine(Duration::from_millis(300), Some(engine_dyn));
        let handle = collector.spawn(tx, token.clone());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut observed: Option<SystemSnapshot> = None;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(snap)) => {
                    if snap.containers.is_some() {
                        observed = Some(snap);
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;

        let snap = observed.expect("no containers-populated snapshot within 5 s");
        let cs = snap.containers.unwrap();
        assert!(!cs.daemon_up);
        assert!(cs.containers.is_empty());
        assert_eq!(cs.engine, EngineKind::Unknown);
        // Ensure the engine was actually called.
        assert!(engine.call_count.load(Ordering::Relaxed) >= 1);
    }

    // ─── GPU integration tests (v0.5) ─────────────────────────────────────

    /// Test double for `GpuEngine`, mirroring `MockEngine` above.
    struct MockGpuEngine {
        call_count: AtomicUsize,
        fail_mode: AtomicBool,
    }

    impl MockGpuEngine {
        fn new() -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                fail_mode: AtomicBool::new(false),
            }
        }

        fn sample_device() -> crate::gpu::GpuDeviceSnapshot {
            crate::gpu::GpuDeviceSnapshot {
                index: 0,
                vendor: crate::gpu::GpuVendor::Nvidia,
                backend: crate::gpu::GpuBackend::Nvml,
                name: "Mock GPU".into(),
                bus_id: "0000:01:00.0".into(),
                driver_version: Some("999.99".into()),
                utilization_pct: Some(50.0),
                mem_utilization_pct: Some(20.0),
                mem_used_bytes: Some(1024),
                mem_total_bytes: Some(4096),
                temperature_c: Some(55.0),
                power_watts: Some(100.0),
                power_limit_watts: Some(200.0),
                graphics_clock_mhz: Some(1800),
                memory_clock_mhz: Some(7000),
                fan_pct: Some(30.0),
                encoder_pct: None,
                decoder_pct: None,
                supports_process_stats: true,
            }
        }
    }

    #[async_trait]
    impl crate::gpu_engine::GpuEngine for MockGpuEngine {
        async fn snapshot(&self) -> Result<crate::gpu::GpusSnapshot, crate::gpu_engine::GpuError> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            if self.fail_mode.load(Ordering::Relaxed) {
                return Err(crate::gpu_engine::GpuError::Query("mock failure".into()));
            }
            Ok(crate::gpu::GpusSnapshot {
                backends: vec![crate::gpu::GpuBackend::Nvml],
                available: true,
                devices: vec![Self::sample_device()],
                processes: vec![crate::gpu::GpuProcessSnapshot {
                    pid: std::process::id(),
                    device_index: 0,
                    name: String::new(),
                    kind: crate::gpu::GpuProcessKind::Compute,
                    mem_bytes: Some(512),
                }],
                detail: String::new(),
            })
        }

        fn backend(&self) -> crate::gpu::GpuBackend {
            crate::gpu::GpuBackend::Nvml
        }
    }

    /// Drain snapshots until one satisfies `pred`, or the deadline passes.
    async fn drain_until(
        rx: &mut mpsc::Receiver<SystemSnapshot>,
        secs: u64,
        pred: impl Fn(&SystemSnapshot) -> bool,
    ) -> Option<SystemSnapshot> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(snap)) if pred(&snap) => return Some(snap),
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return None,
            }
        }
    }

    #[tokio::test]
    async fn test_collector_populates_gpu_with_engine() {
        let engine: Arc<dyn crate::gpu_engine::GpuEngine + Send + Sync> =
            Arc::new(MockGpuEngine::new());
        let (tx, mut rx) = mpsc::channel(8);
        let token = CancellationToken::new();
        let collector =
            Collector::with_all_engines(Duration::from_millis(300), None, None, Some(engine));
        let handle = collector.spawn(tx, token.clone());

        let populated = drain_until(&mut rx, 5, |s| s.gpu.is_some()).await;

        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;

        let snap = populated.expect("no snapshot with gpu populated within 5 s");
        let gpu = snap.gpu.unwrap();
        assert!(gpu.available);
        assert_eq!(gpu.devices.len(), 1);
        assert_eq!(gpu.devices[0].name, "Mock GPU");
    }

    /// The collector must name GPU processes from the process table it
    /// already refreshed — the engine deliberately leaves `name` empty.
    #[tokio::test]
    async fn test_collector_resolves_gpu_process_names() {
        let engine: Arc<dyn crate::gpu_engine::GpuEngine + Send + Sync> =
            Arc::new(MockGpuEngine::new());
        let (tx, mut rx) = mpsc::channel(8);
        let token = CancellationToken::new();
        let collector =
            Collector::with_all_engines(Duration::from_millis(300), None, None, Some(engine));
        let handle = collector.spawn(tx, token.clone());

        let populated = drain_until(&mut rx, 5, |s| {
            s.gpu.as_ref().is_some_and(|g| !g.processes.is_empty())
        })
        .await;

        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;

        let snap = populated.expect("no snapshot with gpu processes within 5 s");
        let gpu = snap.gpu.unwrap();
        assert!(
            !gpu.processes[0].name.is_empty(),
            "collector should have resolved the PID to a name"
        );
    }

    /// A failing GPU engine publishes the `unavailable` sentinel carrying the
    /// reason, not an absence — so the UI can explain itself.
    #[tokio::test]
    async fn test_collector_publishes_gpu_unavailable_on_engine_error() {
        let engine = Arc::new(MockGpuEngine::new());
        engine.fail_mode.store(true, Ordering::Relaxed);
        let engine_dyn: Arc<dyn crate::gpu_engine::GpuEngine + Send + Sync> = engine.clone();
        let (tx, mut rx) = mpsc::channel(8);
        let token = CancellationToken::new();
        let collector =
            Collector::with_all_engines(Duration::from_millis(300), None, None, Some(engine_dyn));
        let handle = collector.spawn(tx, token.clone());

        let observed =
            drain_until(&mut rx, 5, |s| s.gpu.as_ref().is_some_and(|g| !g.available)).await;

        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;

        let snap = observed.expect("no gpu-populated snapshot within 5 s");
        let gpu = snap.gpu.unwrap();
        assert!(!gpu.available);
        assert!(gpu.devices.is_empty());
        assert!(
            gpu.detail.contains("mock failure"),
            "the failure reason should reach the UI, got {:?}",
            gpu.detail
        );
        assert!(engine.call_count.load(Ordering::Relaxed) >= 1);
    }

    /// Without a GPU engine the slot is seeded with the `unavailable`
    /// sentinel so the tab renders "no GPU" immediately rather than sitting
    /// on "Waiting…" forever (the `--no-gpu` and remote-client path).
    #[tokio::test]
    async fn test_collector_without_gpu_engine_seeds_unavailable() {
        let (mut rx, handle, token) = make_collector(4);

        let snap = tokio::time::timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("timeout waiting for snapshot")
            .expect("channel closed before first snapshot");

        token.cancel();
        handle.await.expect("collector task panicked");

        let gpu = snap.gpu.expect("gpu slot should be seeded, not None");
        assert!(!gpu.available);
        assert!(gpu.devices.is_empty());
    }

    /// Sanity: without an engine, the published snapshots keep `containers =
    /// None` forever. Guards against accidentally defaulting to `Some(..)`.
    #[tokio::test]
    async fn test_collector_without_engine_keeps_containers_none() {
        let (mut rx, handle, token) = make_collector(4);

        for _ in 0..3 {
            if let Some(snap) = tokio::time::timeout(Duration::from_secs(4), rx.recv())
                .await
                .expect("timeout")
            {
                assert!(
                    snap.containers.is_none(),
                    "unexpected containers snapshot without engine"
                );
            } else {
                break;
            }
        }

        token.cancel();
        handle.await.expect("collector task panicked");
    }
}

use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::containers::ContainersSnapshot;
use crate::gpu::GpusSnapshot;
use crate::kube::KubeSnapshot;
use crate::network::NetworkSnapshot;
use crate::process::ProcessInfo;

/// The machine's host name, resolved once.
///
/// The TUI header shows it so that three muxtop panes in a tmux window are
/// distinguishable. Cached because the value cannot change during the process
/// lifetime and the syscall behind it is not free on every platform.
pub fn host_name() -> Option<&'static str> {
    static HOST_NAME: OnceLock<Option<String>> = OnceLock::new();
    HOST_NAME.get_or_init(sysinfo::System::host_name).as_deref()
}

/// Interned per-core name table (PERF-L2).
///
/// `format!("cpu{i}")` previously ran on every collector tick for every core,
/// burning a few hundred small allocations per second on multi-core hosts.
/// The names never change for the lifetime of the process, so we mint them
/// once and clone the cached `String` on each tick (a `String::clone` for a
/// 4-6-byte payload reuses the small-string fast path on most allocators).
fn core_name(i: usize) -> String {
    static CORE_NAMES: OnceLock<std::sync::RwLock<Vec<String>>> = OnceLock::new();
    let lock = CORE_NAMES.get_or_init(|| std::sync::RwLock::new(Vec::new()));

    // Fast path: read lock + index lookup.
    {
        let table = lock.read().unwrap_or_else(|e| e.into_inner());
        if let Some(name) = table.get(i) {
            return name.clone();
        }
    }

    // Slow path: extend the table (rare — only on the first tick for that core
    // count, and on the first run a single contiguous extend).
    let mut table = lock.write().unwrap_or_else(|e| e.into_inner());
    while table.len() <= i {
        let idx = table.len();
        table.push(format!("cpu{idx}"));
    }
    table[i].clone()
}

/// Join a process's argv into the single string the COMMAND column renders.
///
/// The obvious spelling — `cmd().iter().map(|s| s.to_string_lossy()
/// .into_owned()).collect::<Vec<_>>().join(" ")` — allocates one `String` per
/// argument, plus the `Vec` holding them, plus the joined result, and then
/// frees all but the last. That is `argc + 2` allocations per process per
/// tick, and it ran for every process on the host at the collector's 1 Hz.
///
/// `to_string_lossy` returns a `Cow` that *borrows* whenever the bytes are
/// already valid UTF-8, which is every ordinary command line, so pushing the
/// borrowed slices into one pre-sized buffer leaves exactly one allocation
/// regardless of `argc`. The capacity is a hint rather than an exact figure:
/// invalid UTF-8 expands, since each malformed byte becomes a three-byte
/// replacement character, and the buffer grows on those rare inputs.
fn join_argv(argv: &[std::ffi::OsString]) -> String {
    let Some((first, rest)) = argv.split_first() else {
        return String::new();
    };
    let capacity = argv.iter().map(|arg| arg.len() + 1).sum::<usize>() - 1;
    let mut joined = String::with_capacity(capacity);
    joined.push_str(&first.to_string_lossy());
    for arg in rest {
        joined.push(' ');
        joined.push_str(&arg.to_string_lossy());
    }
    joined
}

/// Per-core CPU snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct CoreSnapshot {
    pub name: String,
    pub usage: f32,
    pub frequency: u64,
}

/// Aggregated CPU snapshot with global usage and per-core data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct CpuSnapshot {
    pub global_usage: f32,
    pub cores: Vec<CoreSnapshot>,
}

/// Memory and swap snapshot (all values in bytes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MemorySnapshot {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

/// System load averages and uptime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct LoadSnapshot {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
    pub uptime_secs: u64,
}

/// Full system snapshot aggregating all subsystems.
///
/// `containers` is `None` whenever the collector runs without a container
/// engine attached, or before the first container tick has completed.
/// Once set, it is `Some(ContainersSnapshot::unavailable())` to report
/// engine failure or `Some(..)` with the current fleet.
///
/// `kube` follows the exact same convention for the v0.4 cluster engine, and
/// `gpu` for the v0.5 GPU engine.
///
/// **Wire-protocol break (v0.4):** the `kube` field is appended to the
/// struct after `containers`, before `timestamp_ms`. bincode is order-
/// sensitive, so this is incompatible with v0.3.x clients.
///
/// **Wire-protocol break (v0.5):** the `gpu` field is appended after `kube`,
/// still before `timestamp_ms`. Same consequence — a v0.4.x client decoding a
/// v0.5 frame reads the GPU bytes as its `timestamp_ms` and fails. Client and
/// server must match on the minor version.
///
/// # Why `Arc` on two of the three engine fields (PERF-02)
///
/// `containers` and `kube` are produced by loops slower than the system tick
/// — 0.5 Hz and 0.2 Hz against a tick of at most 1 Hz — so owning them meant
/// deep-cloning data that had not changed. At the 5 000-pod `ListParams`
/// ceiling, four of every five kube clones were pure waste, each rebuilding
/// three `String`s per pod.
///
/// `gpu` is deliberately left owned: `GPU_INTERVAL` is 1 s and `--refresh`
/// has a 1 s floor, so the GPU engine never produces more slowly than the
/// tick consumes and its clone is never redundant. The snapshot is also a
/// handful of devices rather than thousands of rows.
///
/// `Arc` does not change the wire format: bincode encodes `Arc<T>` through
/// `T::encode`, so the bytes are identical to the owned layout — see the
/// round-trip test in `muxtop-proto`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct SystemSnapshot {
    pub cpu: CpuSnapshot,
    pub memory: MemorySnapshot,
    pub load: LoadSnapshot,
    pub processes: Vec<ProcessInfo>,
    pub networks: NetworkSnapshot,
    pub containers: Option<Arc<ContainersSnapshot>>,
    pub kube: Option<Arc<KubeSnapshot>>,
    pub gpu: Option<GpusSnapshot>,
    /// Milliseconds since Unix epoch.
    pub timestamp_ms: u64,
}

impl SystemSnapshot {
    /// Collect a full system snapshot from sysinfo.
    ///
    /// `containers` and `kube` are passed through verbatim: callers either
    /// supply the latest snapshot from their engines (via [`crate::collector::Collector`])
    /// or `None` when running without one.
    ///
    /// `gpu` is the one exception — it is passed through *enriched*. GPU
    /// backends report process IDs but no names, and this function already
    /// holds a freshly-refreshed process table for the Processes tab, so
    /// resolving PID → name here costs a hash lookup per GPU process instead
    /// of a second full enumeration inside the backend.
    pub fn collect(
        sys: &sysinfo::System,
        networks: &sysinfo::Networks,
        containers: Option<Arc<ContainersSnapshot>>,
        kube: Option<Arc<KubeSnapshot>>,
        gpu: Option<GpusSnapshot>,
    ) -> Self {
        use sysinfo::System as SysSystem;

        let global_usage = sys.global_cpu_usage();
        let cores = sys
            .cpus()
            .iter()
            .enumerate()
            .map(|(i, cpu)| CoreSnapshot {
                name: core_name(i),
                usage: cpu.cpu_usage(),
                frequency: cpu.frequency(),
            })
            .collect();

        let cpu = CpuSnapshot {
            global_usage,
            cores,
        };

        let memory = MemorySnapshot {
            total: sys.total_memory(),
            used: sys.used_memory(),
            available: sys.available_memory(),
            swap_total: sys.total_swap(),
            swap_used: sys.used_swap(),
        };

        let load = {
            let avg = SysSystem::load_average();
            LoadSnapshot {
                one: avg.one,
                five: avg.five,
                fifteen: avg.fifteen,
                uptime_secs: SysSystem::uptime(),
            }
        };

        let total_mem = sys.total_memory();

        let processes = sys
            .processes()
            .iter()
            .map(|(pid, proc_info)| {
                let mem_pct = if total_mem > 0 {
                    ((proc_info.memory() as f64 / total_mem as f64) * 100.0).clamp(0.0, 100.0)
                        as f32
                } else {
                    0.0
                };

                let status = match proc_info.status() {
                    sysinfo::ProcessStatus::Run => "Running",
                    sysinfo::ProcessStatus::Sleep => "Sleeping",
                    sysinfo::ProcessStatus::Idle => "Idle",
                    sysinfo::ProcessStatus::Zombie => "Zombie",
                    sysinfo::ProcessStatus::Stop => "Stopped",
                    _ => "Unknown",
                };

                ProcessInfo {
                    pid: pid.as_u32(),
                    parent_pid: proc_info.parent().map(|p| p.as_u32()),
                    name: proc_info.name().to_string_lossy().into_owned(),
                    command: join_argv(proc_info.cmd()),
                    user: proc_info
                        .user_id()
                        .map(|u| u.to_string())
                        .unwrap_or_default(),
                    cpu_percent: proc_info.cpu_usage(),
                    memory_bytes: proc_info.memory(),
                    memory_percent: mem_pct,
                    status: status.to_string(),
                }
            })
            .collect();

        let networks = NetworkSnapshot::collect(networks);

        // Resolve GPU process names against the table we just refreshed. The
        // lookup goes through `sysinfo` rather than the `processes` vec above
        // so it stays O(1) per PID instead of a linear scan of every process
        // on the host for each GPU client.
        let gpu = gpu.map(|mut g| {
            g.resolve_process_names(|pid| {
                sys.process(sysinfo::Pid::from_u32(pid))
                    .map(|p| p.name().to_string_lossy().into_owned())
            });
            g
        });

        Self {
            cpu,
            memory,
            load,
            processes,
            networks,
            containers,
            kube,
            gpu,
            // A clock set before the epoch is absurd but reachable — an
            // embedded host with no RTC reads 1970 until NTP lands. A system
            // monitor is exactly the tool someone would reach for on a box in
            // that state, so degrade to 0 instead of taking the whole process
            // down. Matches `kube_engine::unix_ms`.
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `join_argv` has to agree with the `Vec` + `join(" ")` it replaced for
    /// every input, not just the common one — the point of the rewrite was to
    /// drop `argc + 2` allocations per process per tick, and it is only worth
    /// having if the rendered COMMAND column is byte-identical.
    fn reference_join(argv: &[std::ffi::OsString]) -> String {
        argv.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn os(parts: &[&str]) -> Vec<std::ffi::OsString> {
        parts.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn join_argv_matches_the_allocating_join_it_replaced() {
        for case in [
            vec![],
            os(&[""]),
            os(&["ls"]),
            os(&["ls", "-la"]),
            os(&["/usr/bin/env", "python3", "-c", "print('hi there')"]),
            os(&["", "", ""]),
            os(&["arg with spaces", "another one"]),
            os(&["héllo", "wörld", "→"]),
        ] {
            assert_eq!(join_argv(&case), reference_join(&case), "argv: {case:?}");
        }
    }

    #[test]
    fn join_argv_is_empty_for_empty_argv() {
        assert_eq!(join_argv(&[]), "");
    }

    #[test]
    fn join_argv_allocates_exactly_the_capacity_it_predicts_for_utf8() {
        // The pre-sized buffer is the whole point: if the prediction were
        // wrong the string would reallocate and the saving would be smaller
        // than claimed. Valid UTF-8 must fit exactly.
        let argv = os(&["/usr/bin/env", "python3", "-c", "print('hi')"]);
        let joined = join_argv(&argv);
        assert_eq!(joined.len(), joined.capacity());
    }

    /// Invalid UTF-8 is the one case the capacity hint under-counts, because
    /// each malformed byte becomes a three-byte replacement character. The
    /// buffer must grow rather than truncate or panic.
    #[cfg(unix)]
    #[test]
    fn join_argv_survives_invalid_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let argv = vec![
            std::ffi::OsString::from_vec(vec![0xff, 0xfe]),
            std::ffi::OsString::from("ok"),
        ];
        assert_eq!(join_argv(&argv), reference_join(&argv));
        assert!(join_argv(&argv).ends_with(" ok"));
    }

    #[test]
    fn test_snapshot_is_send_clone() {
        fn assert_send_clone<T: Send + Clone>() {}
        assert_send_clone::<CoreSnapshot>();
        assert_send_clone::<CpuSnapshot>();
        assert_send_clone::<MemorySnapshot>();
        assert_send_clone::<LoadSnapshot>();
        assert_send_clone::<SystemSnapshot>();
        assert_send_clone::<ProcessInfo>();
    }

    #[test]
    fn test_system_snapshot_from_sysinfo() {
        use sysinfo::System;
        let mut sys = System::new_all();
        // sysinfo needs a refresh to populate CPU data
        std::thread::sleep(std::time::Duration::from_millis(200));
        sys.refresh_all();

        let networks = sysinfo::Networks::new_with_refreshed_list();
        let snap = SystemSnapshot::collect(&sys, &networks, None, None, None);
        assert!(!snap.cpu.cores.is_empty(), "should have CPU cores");
        assert!(snap.memory.total > 0, "should have total memory");
        assert!(!snap.processes.is_empty(), "should have processes");
    }

    #[test]
    fn test_cpu_snapshot_has_global_and_cores() {
        use sysinfo::System;
        let mut sys = System::new_all();
        std::thread::sleep(std::time::Duration::from_millis(200));
        sys.refresh_all();

        let networks = sysinfo::Networks::new_with_refreshed_list();
        let snap = SystemSnapshot::collect(&sys, &networks, None, None, None);
        assert!(
            snap.cpu.global_usage >= 0.0 && snap.cpu.global_usage <= 100.0,
            "global CPU usage should be 0..=100, got {}",
            snap.cpu.global_usage
        );
        for core in &snap.cpu.cores {
            assert!(
                core.usage >= 0.0 && core.usage <= 100.0,
                "core usage should be 0..=100, got {}",
                core.usage
            );
        }
    }

    #[test]
    fn test_memory_snapshot_invariant() {
        use sysinfo::System;
        let mut sys = System::new_all();
        sys.refresh_all();

        let networks = sysinfo::Networks::new_with_refreshed_list();
        let snap = SystemSnapshot::collect(&sys, &networks, None, None, None);
        assert!(snap.memory.total > 0, "total memory should be positive");
        // used + available can slightly exceed total due to kernel accounting
        // but total should be >= used
        assert!(
            snap.memory.total >= snap.memory.used,
            "total ({}) should be >= used ({})",
            snap.memory.total,
            snap.memory.used
        );
    }

    #[test]
    fn test_system_snapshot_has_networks() {
        use sysinfo::System;
        let mut sys = System::new_all();
        std::thread::sleep(std::time::Duration::from_millis(200));
        sys.refresh_all();
        let networks = sysinfo::Networks::new_with_refreshed_list();

        let snap = SystemSnapshot::collect(&sys, &networks, None, None, None);
        assert!(
            !snap.networks.interfaces.is_empty(),
            "should have network interfaces"
        );
        assert_eq!(
            snap.networks.total_rx,
            snap.networks
                .interfaces
                .iter()
                .map(|i| i.bytes_rx)
                .sum::<u64>(),
            "total_rx should be consistent"
        );
    }

    #[test]
    fn test_core_name_returns_stable_label() {
        // PERF-L2: the interned table returns the canonical label and is
        // stable across calls (we don't observe `Arc` identity since the
        // interface returns owned `String`s, but the values must match the
        // legacy `format!("cpu{i}")` exactly so wire-format consumers don't
        // see a regression).
        assert_eq!(core_name(0), "cpu0");
        assert_eq!(core_name(7), "cpu7");
        assert_eq!(core_name(0), "cpu0"); // hits the fast path
    }

    #[test]
    fn test_collect_resolves_gpu_process_names() {
        use crate::gpu::{GpuProcessKind, GpuProcessSnapshot, GpusSnapshot};
        use sysinfo::System;

        let mut sys = System::new_all();
        sys.refresh_all();
        let networks = sysinfo::Networks::new_with_refreshed_list();

        // Our own PID is guaranteed to be in the table we just refreshed.
        let own_pid = std::process::id();
        let mut gpu = GpusSnapshot::unavailable();
        gpu.processes.push(GpuProcessSnapshot {
            pid: own_pid,
            device_index: 0,
            name: String::new(),
            kind: GpuProcessKind::Compute,
            mem_bytes: Some(4096),
        });

        let snap = SystemSnapshot::collect(&sys, &networks, None, None, Some(gpu));
        let resolved = snap.gpu.expect("gpu passed through");
        assert!(
            !resolved.processes[0].name.is_empty(),
            "collect() should have named the current process"
        );
    }

    #[test]
    fn test_collect_without_gpu_keeps_field_none() {
        use sysinfo::System;
        let mut sys = System::new_all();
        sys.refresh_all();
        let networks = sysinfo::Networks::new_with_refreshed_list();

        let snap = SystemSnapshot::collect(&sys, &networks, None, None, None);
        assert!(snap.gpu.is_none());
    }

    #[test]
    fn test_all_structs_are_debug() {
        let core = CoreSnapshot {
            name: "cpu0".into(),
            usage: 50.0,
            frequency: 3600,
        };
        assert!(!format!("{core:?}").is_empty());

        let cpu = CpuSnapshot {
            global_usage: 25.0,
            cores: vec![core],
        };
        assert!(!format!("{cpu:?}").is_empty());

        let mem = MemorySnapshot {
            total: 16_000_000_000,
            used: 8_000_000_000,
            available: 8_000_000_000,
            swap_total: 4_000_000_000,
            swap_used: 1_000_000_000,
        };
        assert!(!format!("{mem:?}").is_empty());

        let load = LoadSnapshot {
            one: 1.5,
            five: 1.2,
            fifteen: 0.8,
            uptime_secs: 3600,
        };
        assert!(!format!("{load:?}").is_empty());
    }
}

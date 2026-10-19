pub mod actions;
pub mod amd_engine;
/// Apple Silicon backend. The FFI half is gated to macOS inside the module;
/// the derivations stay compiled everywhere so their tests run on every CI
/// target.
pub mod apple;
pub mod cluster_engine;
pub mod collector;
pub mod container_engine;
pub mod container_exec;
pub mod containers;
pub mod docker_engine;
mod docker_exec;
mod docker_exec_transport;
pub mod error;
pub mod gpu;
pub mod gpu_engine;
pub mod kube;
pub mod kube_engine;
pub mod network;
/// NVIDIA backend. Absent on targets NVML does not ship for (macOS), which
/// is also why the `nvml-wrapper` dependency is target-gated in `Cargo.toml`.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod nvml_engine;
pub mod process;
pub mod system;

pub use actions::Signal;
#[cfg(target_os = "macos")]
pub use apple::AppleEngine;
pub use cluster_engine::{
    ClusterEngine, ClusterError, KubeconfigSource, detect_kubeconfig, detect_kubeconfig_with,
};
pub use container_engine::{
    ConnectionTarget, ContainerEngine, EngineError, EnvLookup, StdEnv, detect_socket,
};
pub use containers::{ContainerSnapshot, ContainerState, ContainersSnapshot, EngineKind};
pub use docker_engine::DockerEngine;
pub use error::CoreError;
pub use gpu::{
    GpuBackend, GpuDeviceSnapshot, GpuProcessKind, GpuProcessSnapshot, GpuVendor, GpusSnapshot,
};
pub use gpu_engine::{CompositeGpuEngine, GpuEngine, GpuError, detect_gpu_engines};
pub use kube::{
    ClusterKind, DeploymentSnapshot, DeploymentStrategy, KubeSnapshot, NodeSnapshot, NodeStatus,
    PodPhase, PodSnapshot, QosClass,
};
pub use kube_engine::KubeEngine;
pub use network::{NetworkHistory, NetworkInterfaceSnapshot, NetworkSnapshot};

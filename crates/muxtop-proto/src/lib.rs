pub mod error;
pub mod frame;
pub mod logging;
pub mod remote;
pub mod tls;
pub mod wire;

pub use error::ProtoError;
pub use frame::{
    Frame, FrameReader, FrameWriter, MAX_FRAME_SIZE, MSG_ERROR, MSG_HEARTBEAT, MSG_HELLO,
    MSG_SNAPSHOT, MSG_WELCOME, decode_frame, encode_frame,
};
pub use remote::{ConnectionEvent, RemoteCollector, RemoteUpdate};
pub use tls::{RemoteTargetError, parse_remote_target};
pub use wire::WireMessage;

#[cfg(test)]
mod tests {
    use bincode::{config, decode_from_slice, encode_to_vec};
    use muxtop_core::network::{NetworkInterfaceSnapshot, NetworkSnapshot};
    use muxtop_core::process::ProcessInfo;
    use muxtop_core::system::{
        CoreSnapshot, CpuSnapshot, LoadSnapshot, MemorySnapshot, SystemSnapshot,
    };

    fn bincode_config() -> impl bincode::config::Config {
        config::standard()
    }

    fn make_test_process() -> ProcessInfo {
        ProcessInfo {
            pid: 42,
            parent_pid: Some(1),
            name: "test_proc".into(),
            command: "/usr/bin/test --flag".into(),
            user: "root".into(),
            cpu_percent: 12.5,
            memory_bytes: 1_048_576,
            memory_percent: 2.3,
            status: "Running".into(),
        }
    }

    fn make_test_network_iface() -> NetworkInterfaceSnapshot {
        NetworkInterfaceSnapshot {
            name: "eth0".into(),
            bytes_rx: 123_456,
            bytes_tx: 78_901,
            packets_rx: 100,
            packets_tx: 50,
            errors_rx: 0,
            errors_tx: 0,
            mac_address: "00:11:22:33:44:55".into(),
            is_up: true,
        }
    }

    fn make_test_snapshot() -> SystemSnapshot {
        SystemSnapshot {
            cpu: CpuSnapshot {
                global_usage: 45.2,
                cores: vec![
                    CoreSnapshot {
                        name: "cpu0".into(),
                        usage: 50.0,
                        frequency: 3600,
                    },
                    CoreSnapshot {
                        name: "cpu1".into(),
                        usage: 40.4,
                        frequency: 3600,
                    },
                ],
            },
            memory: MemorySnapshot {
                total: 16_000_000_000,
                used: 8_000_000_000,
                available: 8_000_000_000,
                swap_total: 4_000_000_000,
                swap_used: 1_000_000_000,
            },
            load: LoadSnapshot {
                one: 1.5,
                five: 1.2,
                fifteen: 0.8,
                uptime_secs: 3600,
            },
            processes: vec![make_test_process()],
            networks: NetworkSnapshot {
                interfaces: vec![make_test_network_iface()],
                total_rx: 123_456,
                total_tx: 78_901,
            },
            containers: None,
            kube: None,
            gpu: None,
            timestamp_ms: 1_713_200_000_000,
        }
    }

    #[test]
    fn test_process_info_roundtrip() {
        let original = make_test_process();
        let bytes = encode_to_vec(&original, bincode_config()).unwrap();
        let (decoded, _): (ProcessInfo, _) = decode_from_slice(&bytes, bincode_config()).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_network_interface_roundtrip() {
        let original = make_test_network_iface();
        let bytes = encode_to_vec(&original, bincode_config()).unwrap();
        let (decoded, _): (NetworkInterfaceSnapshot, _) =
            decode_from_slice(&bytes, bincode_config()).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_network_snapshot_roundtrip() {
        let original = NetworkSnapshot {
            interfaces: vec![make_test_network_iface()],
            total_rx: 123_456,
            total_tx: 78_901,
        };
        let bytes = encode_to_vec(&original, bincode_config()).unwrap();
        let (decoded, _): (NetworkSnapshot, _) =
            decode_from_slice(&bytes, bincode_config()).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_system_snapshot_roundtrip() {
        let original = make_test_snapshot();
        let bytes = encode_to_vec(&original, bincode_config()).unwrap();
        let (decoded, _): (SystemSnapshot, _) =
            decode_from_slice(&bytes, bincode_config()).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_cpu_snapshot_roundtrip() {
        let original = CpuSnapshot {
            global_usage: 75.3,
            cores: vec![CoreSnapshot {
                name: "cpu0".into(),
                usage: 75.3,
                frequency: 2400,
            }],
        };
        let bytes = encode_to_vec(&original, bincode_config()).unwrap();
        let (decoded, _): (CpuSnapshot, _) = decode_from_slice(&bytes, bincode_config()).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_memory_snapshot_roundtrip() {
        let original = MemorySnapshot {
            total: 32_000_000_000,
            used: 16_000_000_000,
            available: 16_000_000_000,
            swap_total: 8_000_000_000,
            swap_used: 2_000_000_000,
        };
        let bytes = encode_to_vec(&original, bincode_config()).unwrap();
        let (decoded, _): (MemorySnapshot, _) =
            decode_from_slice(&bytes, bincode_config()).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_load_snapshot_roundtrip() {
        let original = LoadSnapshot {
            one: 2.5,
            five: 1.8,
            fifteen: 1.2,
            uptime_secs: 86400,
        };
        let bytes = encode_to_vec(&original, bincode_config()).unwrap();
        let (decoded, _): (LoadSnapshot, _) = decode_from_slice(&bytes, bincode_config()).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_timestamp_is_unix_ms() {
        let snap = make_test_snapshot();
        assert!(snap.timestamp_ms > 0, "timestamp should be positive");
        assert!(
            snap.timestamp_ms > 1_700_000_000_000,
            "timestamp should be in milliseconds since epoch"
        );
    }
}

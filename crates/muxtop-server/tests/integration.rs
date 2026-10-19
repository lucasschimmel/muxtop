//! Production TCP/TLS server integration tests.
// Production modules are shared directly; no test server or copied handler.
#[path = "../src/client.rs"]
pub mod client;
#[path = "../src/error.rs"]
pub mod error;
#[path = "../src/rate_limit.rs"]
pub mod rate_limit;
#[path = "../src/server.rs"]
pub mod server;

use muxtop_core::network::{NetworkInterfaceSnapshot, NetworkSnapshot};
use muxtop_core::process::ProcessInfo;
use muxtop_core::system::{
    CoreSnapshot, CpuSnapshot, LoadSnapshot, MemorySnapshot, SystemSnapshot,
};
use muxtop_proto::{FrameReader, FrameWriter, WireMessage};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_util::sync::CancellationToken;

fn make_snapshot() -> SystemSnapshot {
    SystemSnapshot {
        cpu: CpuSnapshot {
            global_usage: 25.0,
            cores: vec![CoreSnapshot {
                name: "cpu0".into(),
                usage: 25.0,
                frequency: 3600,
            }],
        },
        memory: MemorySnapshot {
            total: 16_000_000_000,
            used: 8_000_000_000,
            available: 8_000_000_000,
            swap_total: 4_000_000_000,
            swap_used: 1_000_000_000,
        },
        load: LoadSnapshot {
            one: 1.0,
            five: 0.8,
            fifteen: 0.5,
            uptime_secs: 3600,
        },
        processes: vec![ProcessInfo {
            pid: 1,
            parent_pid: None,
            name: "init".into(),
            command: "/sbin/init".into(),
            user: "root".into(),
            cpu_percent: 0.1,
            memory_bytes: 4096,
            memory_percent: 0.01,
            status: "Running".into(),
        }],
        networks: NetworkSnapshot {
            interfaces: vec![NetworkInterfaceSnapshot {
                name: "lo".into(),
                bytes_rx: 1000,
                bytes_tx: 1000,
                packets_rx: 10,
                packets_tx: 10,
                errors_rx: 0,
                errors_tx: 0,
                mac_address: "00:00:00:00:00:00".into(),
                is_up: true,
            }],
            total_rx: 1000,
            total_tx: 1000,
        },
        containers: None,
        kube: None,
        gpu: None,
        timestamp_ms: 1_713_200_000_000,
    }
}

struct Harness {
    addr: std::net::SocketAddr,
    snapshots: mpsc::Sender<SystemSnapshot>,
    connector: TlsConnector,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Harness {
    async fn start(max_clients: usize) -> Self {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let der = CertificateDer::from(cert.cert.der().to_vec());
        let key = PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap();
        let tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![der.clone()], key)
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(der).unwrap();
        let connector = TlsConnector::from(Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = server::ServerConfig {
            bind: addr,
            max_clients,
            auth_token: "test-auth-placeholder".into(),
            refresh_hz: 1,
            tls_acceptor: TlsAcceptor::from(Arc::new(tls)),
            rate_limit_per_ip: 0.0,
        };
        let (snapshots, rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(server::run_with_listener(
            config,
            listener,
            rx,
            cancel.clone(),
        ));
        Self {
            addr,
            snapshots,
            connector,
            cancel,
            handle,
        }
    }

    async fn connect(&self) -> tokio_rustls::client::TlsStream<TcpStream> {
        self.connector
            .connect(
                ServerName::try_from("localhost").unwrap(),
                TcpStream::connect(self.addr).await.unwrap(),
            )
            .await
            .unwrap()
    }

    async fn stop(self) {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), self.handle)
            .await
            .expect("server shutdown must join all sessions")
            .unwrap()
            .unwrap();
    }
}

async fn hello<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: S,
    auth: &str,
) -> (
    FrameReader<tokio::io::ReadHalf<S>>,
    FrameWriter<tokio::io::WriteHalf<S>>,
    WireMessage,
) {
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = FrameReader::new(reader);
    let mut writer = FrameWriter::new(writer);
    writer
        .write_frame(
            &WireMessage::Hello {
                client_version: "test".into(),
                auth_token: Some(auth.into()),
            }
            .to_frame()
            .unwrap(),
        )
        .await
        .unwrap();
    let frame = reader.read_frame().await.unwrap().unwrap();
    (reader, writer, WireMessage::from_frame(&frame).unwrap())
}

// @generated by:gauntlet run:muxtop-audit-fixes
#[tokio::test]
async fn t_ac_06_production_server_authenticates_and_relays_complete_snapshots() {
    let server = Harness::start(2).await;
    let (mut reader, _writer, response) =
        hello(server.connect().await, "test-auth-placeholder").await;
    assert!(matches!(response, WireMessage::Welcome { .. }));
    let expected = make_snapshot();
    server.snapshots.send(expected.clone()).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), reader.read_frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        frame,
        WireMessage::Snapshot(expected.clone()).to_frame().unwrap()
    );
    let WireMessage::Snapshot(actual) = WireMessage::from_frame(&frame).unwrap() else {
        panic!("expected snapshot")
    };
    assert_eq!(actual.timestamp_ms, expected.timestamp_ms);
    assert_eq!(actual.processes[0].name, expected.processes[0].name);
    assert_eq!(actual.memory.total, expected.memory.total);
    server.stop().await;
    let closed = tokio::time::timeout(Duration::from_secs(1), reader.read_frame())
        .await
        .unwrap();
    assert!(
        !matches!(closed, Ok(Some(_))),
        "client must observe server shutdown"
    );
}

// @generated by:gauntlet run:muxtop-audit-fixes
#[tokio::test]
async fn t_ac_06_production_server_rejects_wrong_authentication() {
    let server = Harness::start(1).await;
    let (_, _, response) = hello(server.connect().await, "wrong-placeholder").await;
    assert!(matches!(response, WireMessage::Error { code: 401, .. }));
    server.stop().await;
}

// @generated by:gauntlet run:muxtop-audit-fixes
#[tokio::test]
async fn t_ac_06_production_server_caps_pre_auth_payload() {
    use tokio::io::AsyncWriteExt;
    let server = Harness::start(1).await;
    let mut stream = server.connect().await;
    stream
        .write_all(&(5u32 * 1024 + 1).to_be_bytes())
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let mut reader = FrameReader::new(stream);
    let result = tokio::time::timeout(Duration::from_secs(1), reader.read_frame())
        .await
        .unwrap();
    assert!(
        !matches!(result, Ok(Some(_))),
        "oversize Hello must close before accepting payload"
    );
    server.stop().await;
}

// @generated by:gauntlet run:muxtop-audit-fixes
#[tokio::test]
async fn t_ac_04_shutdown_cancels_pending_tls_and_hello_handshakes() {
    let server = Harness::start(2).await;
    let _hello_pending = server.connect().await;
    let _tls_pending = TcpStream::connect(server.addr).await.unwrap();
    server.stop().await;
}

// @generated by:gauntlet run:muxtop-audit-fixes
#[tokio::test]
async fn t_ac_06_server_rejects_untrusted_certificate() {
    let server = Harness::start(1).await;
    let connector = TlsConnector::from(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth(),
    ));
    let result = connector
        .connect(
            ServerName::try_from("localhost").unwrap(),
            TcpStream::connect(server.addr).await.unwrap(),
        )
        .await;
    assert!(result.is_err());
    server.stop().await;
}

// @generated by:gauntlet run:muxtop-audit-fixes
#[tokio::test]
async fn t_ac_06_capacity_is_enforced_before_tls_handshake() {
    use tokio::io::AsyncReadExt;
    let server = Harness::start(1).await;
    // A successful TLS handshake establishes that the first permit is held,
    // while deliberately withholding Hello keeps that slot occupied.
    let _pending = server.connect().await;
    let mut refused = TcpStream::connect(server.addr).await.unwrap();
    let mut byte = [0u8; 1];
    let result = tokio::time::timeout(Duration::from_secs(1), refused.read(&mut byte))
        .await
        .unwrap();
    assert!(
        matches!(result, Ok(0) | Err(_)),
        "full server must close without TLS work"
    );
    server.stop().await;
}

// @generated by:gauntlet run:muxtop-audit-fixes
#[tokio::test]
async fn t_ac_06_production_server_fans_out_multiple_snapshots_to_multiple_clients() {
    let server = Harness::start(2).await;
    let (mut first, _first_writer, _) =
        hello(server.connect().await, "test-auth-placeholder").await;
    let (mut second, _second_writer, _) =
        hello(server.connect().await, "test-auth-placeholder").await;
    for timestamp in 1..=3 {
        let mut snapshot = make_snapshot();
        snapshot.timestamp_ms = timestamp;
        server.snapshots.send(snapshot).await.unwrap();
    }
    for timestamp in 1..=3 {
        for reader in [&mut first, &mut second] {
            let frame = tokio::time::timeout(Duration::from_secs(1), reader.read_frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(
                matches!(WireMessage::from_frame(&frame).unwrap(), WireMessage::Snapshot(s) if s.timestamp_ms == timestamp)
            );
        }
    }
    server.stop().await;
}

// @generated by:gauntlet run:muxtop-audit-fixes
#[tokio::test]
async fn t_ac_06_explicit_skip_verify_connector_streams_from_production_server() {
    let server = Harness::start(1).await;
    let connector = muxtop_proto::tls::connector_insecure();
    let stream = connector
        .connect(
            ServerName::try_from("localhost").unwrap(),
            TcpStream::connect(server.addr).await.unwrap(),
        )
        .await
        .unwrap();
    let (_, _, welcome) = hello(stream, "test-auth-placeholder").await;
    assert!(matches!(welcome, WireMessage::Welcome { .. }));
    server.stop().await;
}

// @generated by:gauntlet run:muxtop-audit-fixes
#[tokio::test]
async fn t_ac_06_production_server_rejects_missing_authentication() {
    let server = Harness::start(1).await;
    let (reader, writer) = tokio::io::split(server.connect().await);
    let mut writer = FrameWriter::new(writer);
    writer
        .write_frame(
            &WireMessage::Hello {
                client_version: "test".into(),
                auth_token: None,
            }
            .to_frame()
            .unwrap(),
        )
        .await
        .unwrap();
    let frame = FrameReader::new(reader)
        .read_frame()
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        WireMessage::from_frame(&frame).unwrap(),
        WireMessage::Error { code: 401, .. }
    ));
    server.stop().await;
}

// Remote collector: TCP client that receives snapshots from muxtop-server via TLS.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;

use crate::{FrameReader, FrameWriter, WireMessage};

use muxtop_core::system::SystemSnapshot;

/// Maximum backoff delay between reconnection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Initial backoff delay.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Timeout for the initial handshake (Hello/Welcome exchange).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum server keepalive gap, independent of the configured snapshot cadence.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = HEARTBEAT_INTERVAL.saturating_mul(3);

// Keep the public error enum stable while distinguishing a lost live session
// from a TCP connection timeout when choosing the reconnect backoff.
#[derive(Debug, thiserror::Error)]
#[error("server stopped sending snapshots and heartbeats")]
struct IdleTimeout;

/// Connection event sent to the TUI for status display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionEvent {
    /// Successfully connected (or reconnected) to server.
    Connected { hostname: String },
    /// Connection lost — reconnection will be attempted.
    Disconnected,
    /// Server sent an error (e.g. 401 unauthorized, 503 max clients).
    ServerError { code: u16, message: String },
}

/// Ordered local updates keep connection transitions and samples in one FIFO.
// Keep snapshots inline, as in WireMessage, to avoid an allocation per sample.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum RemoteUpdate {
    Connection(ConnectionEvent),
    Snapshot(SystemSnapshot),
}

enum RemoteOutput {
    Split {
        tx: mpsc::Sender<SystemSnapshot>,
        conn_tx: Option<mpsc::Sender<ConnectionEvent>>,
    },
    Ordered(mpsc::Sender<RemoteUpdate>),
}

impl RemoteOutput {
    async fn closed(&self) {
        match self {
            Self::Split { tx, .. } => tx.closed().await,
            Self::Ordered(tx) => tx.closed().await,
        }
    }

    fn snapshot(&self, snapshot: SystemSnapshot) -> bool {
        match self {
            Self::Split { tx, .. } => !matches!(
                tx.try_send(snapshot),
                Err(mpsc::error::TrySendError::Closed(_))
            ),
            Self::Ordered(tx) => !matches!(
                tx.try_send(RemoteUpdate::Snapshot(snapshot)),
                Err(mpsc::error::TrySendError::Closed(_))
            ),
        }
    }

    async fn event(&self, event: ConnectionEvent, cancel: &CancellationToken) {
        match self {
            Self::Split {
                conn_tx: Some(tx), ..
            } => {
                let _ = tx.try_send(event);
            }
            Self::Split { conn_tx: None, .. } => {}
            Self::Ordered(tx) => {
                // Never lose transitions behind dropped samples. Cancellation
                // interrupts backpressure when the UI has stopped consuming.
                tokio::select! {
                    _ = cancel.cancelled() => {},
                    _ = tx.send(RemoteUpdate::Connection(event)) => {},
                }
            }
        }
    }
}

/// A TLS client that connects to a muxtop-server and receives snapshots.
pub struct RemoteCollector {
    addr: SocketAddr,
    token: Option<String>,
    tls_connector: TlsConnector,
    server_name: rustls_pki_types::ServerName<'static>,
}

impl RemoteCollector {
    /// Create a new remote collector targeting the given server address.
    pub fn new(
        addr: SocketAddr,
        token: Option<String>,
        tls_connector: TlsConnector,
        server_name: rustls_pki_types::ServerName<'static>,
    ) -> Self {
        Self {
            addr,
            token,
            tls_connector,
            server_name,
        }
    }

    /// Spawn the remote collector as a background tokio task.
    ///
    /// This has the same signature pattern as [`muxtop_core::collector::Collector::spawn`]:
    /// it sends `SystemSnapshot` values into `tx` and shuts down when `cancel` fires.
    ///
    /// An optional `conn_tx` channel receives [`ConnectionEvent`] notifications
    /// so the TUI can display connection status.
    pub fn spawn(
        self,
        tx: mpsc::Sender<SystemSnapshot>,
        conn_tx: Option<mpsc::Sender<ConnectionEvent>>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run(RemoteOutput::Split { tx, conn_tx }, cancel))
    }

    /// Spawn with ordered connection events and snapshots for UI consumers.
    pub fn spawn_updates(
        self,
        tx: mpsc::Sender<RemoteUpdate>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run(RemoteOutput::Ordered(tx), cancel))
    }

    async fn run(self, output: RemoteOutput, cancel: CancellationToken) {
        let mut backoff = INITIAL_BACKOFF;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            match self.connect_and_stream(&output, &cancel).await {
                Ok(()) => {
                    // Clean shutdown (cancel fired).
                    break;
                }
                Err(e) => {
                    tracing::warn!("remote connection error: {e}");
                    output.event(ConnectionEvent::Disconnected, &cancel).await;

                    // Wait with exponential backoff, but respect cancellation.
                    tokio::select! {
                        () = tokio::time::sleep(backoff) => {}
                        () = cancel.cancelled() => break,
                    }

                    // G-01: Only grow backoff for consecutive failures.
                    // ConnectionClosed means we had a successful connection that dropped,
                    // so reset backoff for a fresh reconnection attempt.
                    let session_ended = matches!(&e, RemoteError::ConnectionClosed)
                        || matches!(&e, RemoteError::Io(error) if error.get_ref().is_some_and(|cause| cause.is::<IdleTimeout>()));
                    if session_ended {
                        backoff = INITIAL_BACKOFF;
                    } else {
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                }
            }
        }

        tracing::debug!("remote collector shutting down");
    }

    /// Connect to the server, perform handshake, and stream snapshots.
    ///
    /// Returns `Ok(())` only when cancel is fired (clean shutdown).
    /// Returns `Err` on any connection/protocol error (caller will reconnect).
    async fn connect_and_stream(
        &self,
        output: &RemoteOutput,
        cancel: &CancellationToken,
    ) -> Result<(), RemoteError> {
        // Connect with cancellation support.
        let tcp_stream = tokio::select! {
            result = TcpStream::connect(self.addr) => result?,
            () = cancel.cancelled() => return Ok(()),
        };

        // TLS handshake (with timeout and cancellation).
        let tls_stream = tokio::select! {
            result = timeout(HANDSHAKE_TIMEOUT, self.tls_connector.connect(self.server_name.clone(), tcp_stream)) => {
                result
                    .map_err(|_| RemoteError::Protocol("TLS handshake timed out".into()))?
                    .map_err(|e| RemoteError::Protocol(format!("TLS handshake failed: {e}")))?
            }
            () = cancel.cancelled() => return Ok(()),
        };

        let (reader, writer) = tokio::io::split(tls_stream);
        self.stream_session(reader, writer, output, cancel).await
    }

    async fn stream_session<R, W>(
        &self,
        reader: R,
        writer: W,
        output: &RemoteOutput,
        cancel: &CancellationToken,
    ) -> Result<(), RemoteError>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut frame_reader = FrameReader::new(reader);
        let mut frame_writer = FrameWriter::new(writer);

        // --- Handshake ---
        let hello = WireMessage::Hello {
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            auth_token: self.token.clone(),
        };
        let hello_frame = hello
            .to_frame()
            .map_err(|e| RemoteError::Protocol(e.to_string()))?;
        let welcome_frame = tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = output.closed() => return Ok(()),
            result = timeout(HANDSHAKE_TIMEOUT, async {
                frame_writer.write_frame(&hello_frame).await
                    .map_err(|e| RemoteError::Protocol(e.to_string()))?;
                frame_reader.read_frame().await
                    .map_err(|e| RemoteError::Protocol(e.to_string()))?
                    .ok_or(RemoteError::ConnectionClosed)
            }) => result.map_err(|_| RemoteError::HandshakeTimeout)??,
        };

        let welcome_msg = WireMessage::from_frame(&welcome_frame)
            .map_err(|e| RemoteError::Protocol(e.to_string()))?;

        let hostname = match welcome_msg {
            WireMessage::Welcome { hostname, .. } => hostname,
            WireMessage::Error { code, message } => {
                output
                    .event(
                        ConnectionEvent::ServerError {
                            code,
                            message: message.clone(),
                        },
                        cancel,
                    )
                    .await;
                return Err(RemoteError::ServerError { code, message });
            }
            other => {
                return Err(RemoteError::Protocol(format!(
                    "expected Welcome, got {other:?}"
                )));
            }
        };

        tracing::info!("connected to {hostname} at {}", self.addr);
        output
            .event(ConnectionEvent::Connected { hostname }, cancel)
            .await;

        // --- Streaming loop ---
        loop {
            tokio::select! {
                frame_result = timeout(IDLE_TIMEOUT, frame_reader.read_frame()) => {
                    let frame = match frame_result.map_err(|_| {
                        RemoteError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            IdleTimeout,
                        ))
                    })? {
                        Ok(Some(f)) => f,
                        Ok(None) => return Err(RemoteError::ConnectionClosed),
                        Err(e) => return Err(RemoteError::Protocol(e.to_string())),
                    };

                    let msg = WireMessage::from_frame(&frame)
                        .map_err(|e| RemoteError::Protocol(e.to_string()))?;

                    match msg {
                        WireMessage::Snapshot(snapshot) => {
                            if !output.snapshot(snapshot) {
                                return Ok(());
                            }
                        }
                        WireMessage::Heartbeat { .. } => {
                            tracing::trace!("heartbeat received");
                        }
                        WireMessage::Error { code, message } => {
                            tracing::error!("server error {code}: {message}");
                            output.event(ConnectionEvent::ServerError { code, message: message.clone() }, cancel).await;
                            return Err(RemoteError::ServerError { code, message });
                        }
                        other => {
                            tracing::warn!("unexpected message: {other:?}");
                        }
                    }
                }
                () = output.closed() => return Ok(()),
                () = cancel.cancelled() => {
                    tracing::debug!("remote collector cancelled");
                    return Ok(());
                }
            }
        }
    }
}

/// Errors from the remote collector (internal, not exposed to users).
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("handshake timeout")]
    HandshakeTimeout,

    #[error("connection closed")]
    ConnectionClosed,

    #[error("server error {code}: {message}")]
    ServerError { code: u16, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::connector_insecure;
    use crate::{FrameReader, FrameWriter, WireMessage};
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;

    /// Generate self-signed cert and build a TLS acceptor for testing.
    fn test_tls_acceptor() -> TlsAcceptor {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_pem = ck.cert.pem();
        let key_pem = ck.signing_key.serialize_pem();

        let certs: Vec<CertificateDer<'static>> =
            CertificateDer::pem_slice_iter(cert_pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();

        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();

        TlsAcceptor::from(Arc::new(config))
    }

    /// Build a test RemoteCollector with insecure TLS (skip verify).
    fn test_collector(addr: SocketAddr, token: Option<String>) -> RemoteCollector {
        let tls_connector = connector_insecure();
        let server_name = rustls_pki_types::ServerName::IpAddress(addr.ip().into());
        RemoteCollector::new(addr, token, tls_connector, server_name)
    }

    /// Helper: start a TLS mock server that sends Welcome then streams one snapshot.
    async fn mock_server(auth_token: Option<&str>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let tls_acceptor = test_tls_acceptor();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected_token = auth_token.map(String::from);

        let handle = tokio::spawn(async move {
            let (tcp_stream, _) = listener.accept().await.unwrap();
            let tls_stream = tls_acceptor.accept(tcp_stream).await.unwrap();
            let (reader, writer) = tokio::io::split(tls_stream);
            let mut frame_reader = FrameReader::new(reader);
            let mut frame_writer = FrameWriter::new(writer);

            // Read Hello.
            let frame = frame_reader.read_frame().await.unwrap().unwrap();
            let msg = WireMessage::from_frame(&frame).unwrap();
            match msg {
                WireMessage::Hello { auth_token, .. } => {
                    if let Some(ref expected) = expected_token
                        && auth_token.as_deref() != Some(expected.as_str())
                    {
                        let err = WireMessage::Error {
                            code: 401,
                            message: "unauthorized".to_string(),
                        };
                        let f = err.to_frame().unwrap();
                        frame_writer.write_frame(&f).await.unwrap();
                        return;
                    }
                }
                _ => panic!("expected Hello"),
            }

            // Send Welcome.
            let welcome = WireMessage::Welcome {
                server_version: "0.1.1".to_string(),
                hostname: "test-host".to_string(),
                refresh_hz: 1,
            };
            let f = welcome.to_frame().unwrap();
            frame_writer.write_frame(&f).await.unwrap();

            // Send one snapshot.
            let snapshot = make_test_snapshot();
            let snap_msg = WireMessage::Snapshot(snapshot);
            let f = snap_msg.to_frame().unwrap();
            frame_writer.write_frame(&f).await.unwrap();

            // Keep connection alive briefly.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        (addr, handle)
    }

    fn make_test_snapshot() -> SystemSnapshot {
        use muxtop_core::network::{NetworkInterfaceSnapshot, NetworkSnapshot};
        use muxtop_core::process::ProcessInfo;
        use muxtop_core::system::{CoreSnapshot, CpuSnapshot, LoadSnapshot, MemorySnapshot};

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

    // @generated by:gauntlet run:muxtop-audit-fixes
    #[tokio::test]
    async fn t_ac_04_spawn_updates_reconnects_after_silence_and_receives_fresh_data() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = test_tls_acceptor();
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let first = acceptor.accept(tcp).await.unwrap();
            let (first_reader, first_writer) = tokio::io::split(first);
            let mut first_reader = FrameReader::new(first_reader);
            let mut first_writer = FrameWriter::new(first_writer);
            let hello = first_reader.read_frame().await.unwrap().unwrap();
            assert!(matches!(
                WireMessage::from_frame(&hello).unwrap(),
                WireMessage::Hello { .. }
            ));
            first_writer
                .write_frame(
                    &WireMessage::Welcome {
                        server_version: "test".into(),
                        hostname: "silent-host".into(),
                        refresh_hz: 1,
                    }
                    .to_frame()
                    .unwrap(),
                )
                .await
                .unwrap();
            // Keep the first connection open and completely silent while
            // accepting a genuine reconnect on the same listening socket.
            let (tcp, _) = listener.accept().await.unwrap();
            let second = acceptor.accept(tcp).await.unwrap();
            let (reader, writer) = tokio::io::split(second);
            let mut reader = FrameReader::new(reader);
            let mut writer = FrameWriter::new(writer);
            let hello = reader.read_frame().await.unwrap().unwrap();
            assert!(matches!(
                WireMessage::from_frame(&hello).unwrap(),
                WireMessage::Hello { .. }
            ));
            writer
                .write_frame(
                    &WireMessage::Welcome {
                        server_version: "test".into(),
                        hostname: "recovered-host".into(),
                        refresh_hz: 1,
                    }
                    .to_frame()
                    .unwrap(),
                )
                .await
                .unwrap();
            let mut snapshot = make_test_snapshot();
            snapshot.timestamp_ms = 42;
            writer
                .write_frame(&WireMessage::Snapshot(snapshot).to_frame().unwrap())
                .await
                .unwrap();
            server_cancel.cancelled().await;
            // Both halves must stay alive until the test explicitly stops the server.
            drop((first_reader, first_writer, reader, writer));
        });
        let (tx, mut updates) = mpsc::channel(8);
        let collector = test_collector(addr, None).spawn_updates(tx, cancel.clone());
        let connected = timeout(Duration::from_secs(3), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(connected, RemoteUpdate::Connection(ConnectionEvent::Connected { hostname }) if hostname == "silent-host")
        );

        // Advance only the quiet period and retry delay under controlled
        // time. Resume before real socket/TLS I/O to avoid virtual-time
        // handshake deadlines racing the operating system's readiness.
        tokio::time::pause();
        tokio::task::yield_now().await;
        tokio::time::advance(IDLE_TIMEOUT).await;
        let disconnected = timeout(Duration::from_secs(1), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            disconnected,
            RemoteUpdate::Connection(ConnectionEvent::Disconnected)
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_BACKOFF).await;
        tokio::time::resume();

        let reconnected = timeout(Duration::from_secs(3), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(reconnected, RemoteUpdate::Connection(ConnectionEvent::Connected { hostname }) if hostname == "recovered-host")
        );
        let snapshot = timeout(Duration::from_secs(3), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(snapshot, RemoteUpdate::Snapshot(snapshot) if snapshot.timestamp_ms == 42)
        );
        cancel.cancel();
        timeout(Duration::from_secs(1), collector)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }

    // @generated by:gauntlet run:muxtop-audit-fixes
    #[tokio::test(start_paused = true)]
    async fn t_ac_04_ordered_status_backpressure_is_cancellable() {
        let (tx, _rx) = mpsc::channel(1);
        let output = RemoteOutput::Ordered(tx);
        assert!(output.snapshot(make_test_snapshot()));
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            output
                .event(ConnectionEvent::Disconnected, &task_cancel)
                .await;
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        cancel.cancel();
        task.await.unwrap();
    }

    async fn session_fixture() -> (
        tokio::task::JoinHandle<Result<(), RemoteError>>,
        FrameWriter<tokio::io::DuplexStream>,
        CancellationToken,
        mpsc::Receiver<SystemSnapshot>,
    ) {
        let (client, server) = tokio::io::duplex(4096);
        let mut server = FrameWriter::new(server);
        server
            .write_frame(
                &WireMessage::Welcome {
                    hostname: "test-host".into(),
                    server_version: "test".into(),
                    refresh_hz: 3600,
                }
                .to_frame()
                .unwrap(),
            )
            .await
            .unwrap();
        let (reader, writer) = tokio::io::split(client);
        let (tx, rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let collector = test_collector("127.0.0.1:1".parse().unwrap(), None);
        let task = tokio::spawn(async move {
            collector
                .stream_session(
                    reader,
                    writer,
                    &RemoteOutput::Split { tx, conn_tx: None },
                    &task_cancel,
                )
                .await
        });
        tokio::task::yield_now().await;
        (task, server, cancel, rx)
    }

    // @generated by:gauntlet run:muxtop-audit-fixes
    #[tokio::test(start_paused = true)]
    async fn t_ac_04_welcome_followed_by_silence_expires() {
        let (task, _server, _cancel, _rx) = session_fixture().await;
        tokio::time::advance(IDLE_TIMEOUT + Duration::from_secs(1)).await;
        assert!(matches!(
            task.await.unwrap(),
            Err(RemoteError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    // @generated by:gauntlet run:muxtop-audit-fixes
    #[tokio::test(start_paused = true)]
    async fn t_ac_04_heartbeats_keep_slow_snapshot_session_live_and_snapshot_recovers() {
        let (task, mut server, cancel, mut rx) = session_fixture().await;
        for _ in 0..5 {
            tokio::time::advance(HEARTBEAT_INTERVAL).await;
            server
                .write_frame(
                    &WireMessage::Heartbeat {
                        server_version: "test".into(),
                        uptime_secs: 1,
                    }
                    .to_frame()
                    .unwrap(),
                )
                .await
                .unwrap();
            tokio::task::yield_now().await;
            assert!(!task.is_finished());
        }
        server
            .write_frame(
                &WireMessage::Snapshot(make_test_snapshot())
                    .to_frame()
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rx.recv().await.unwrap().cpu.global_usage, 25.0);
        cancel.cancel();
        assert!(task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_remote_collector_handshake() {
        let (addr, server) = mock_server(None).await;

        let (tx, mut rx) = mpsc::channel(4);
        let (conn_tx, mut conn_rx) = mpsc::channel(4);
        let token = CancellationToken::new();

        let collector = test_collector(addr, None);
        let handle = collector.spawn(tx, Some(conn_tx), token.clone());

        // Should receive Connected event.
        let event = tokio::time::timeout(Duration::from_secs(3), conn_rx.recv())
            .await
            .expect("timeout waiting for connection event")
            .expect("channel closed");
        assert_eq!(
            event,
            ConnectionEvent::Connected {
                hostname: "test-host".to_string()
            }
        );

        // Should receive a snapshot.
        let snap = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timeout waiting for snapshot")
            .expect("channel closed");
        assert!(!snap.processes.is_empty());

        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        server.abort();
    }

    #[tokio::test]
    async fn test_remote_collector_receives_snapshots() {
        let (addr, server) = mock_server(None).await;

        let (tx, mut rx) = mpsc::channel(4);
        let token = CancellationToken::new();

        let collector = test_collector(addr, None);
        let handle = collector.spawn(tx, None, token.clone());

        let snap = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        assert_eq!(snap.cpu.global_usage, 25.0);
        assert_eq!(snap.processes.len(), 1);
        assert_eq!(snap.processes[0].name, "init");

        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        server.abort();
    }

    #[tokio::test]
    async fn test_remote_collector_shutdown() {
        let (addr, server) = mock_server(None).await;

        let (tx, _rx) = mpsc::channel(4);
        let token = CancellationToken::new();

        let collector = test_collector(addr, None);
        let handle = collector.spawn(tx, None, token.clone());

        // Wait briefly for connection, then cancel.
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();

        // Should complete within 2 seconds.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("collector did not shut down")
            .expect("collector panicked");

        server.abort();
    }

    #[tokio::test]
    async fn test_remote_collector_auth_failure() {
        let (addr, server) = mock_server(Some("correct-token")).await;

        let (tx, _rx) = mpsc::channel(4);
        let (conn_tx, mut conn_rx) = mpsc::channel(4);
        let token = CancellationToken::new();

        let collector = test_collector(addr, Some("wrong-token".to_string()));
        let handle = collector.spawn(tx, Some(conn_tx), token.clone());

        // Should receive ServerError event (401).
        let event = tokio::time::timeout(Duration::from_secs(3), conn_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(
            matches!(event, ConnectionEvent::ServerError { code: 401, .. }),
            "expected 401, got {event:?}"
        );

        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        server.abort();
    }

    #[tokio::test]
    async fn test_remote_collector_reconnect_backoff() {
        // No server running — connection should fail and retry with backoff.
        let addr: SocketAddr = "127.0.0.1:19999".parse().unwrap();

        let (tx, _rx) = mpsc::channel(4);
        let (conn_tx, mut conn_rx) = mpsc::channel(16);
        let token = CancellationToken::new();

        let collector = test_collector(addr, None);
        let handle = collector.spawn(tx, Some(conn_tx), token.clone());

        // Should get Disconnected events as connection attempts fail.
        let event = tokio::time::timeout(Duration::from_secs(3), conn_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert_eq!(event, ConnectionEvent::Disconnected);

        // Cancel before too many retries.
        token.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("collector did not shut down")
            .expect("collector panicked");
    }

    #[test]
    fn test_remote_error_display() {
        let errors: Vec<RemoteError> = vec![
            RemoteError::Io(std::io::Error::other("test")),
            RemoteError::Protocol("bad frame".to_string()),
            RemoteError::HandshakeTimeout,
            RemoteError::ConnectionClosed,
            RemoteError::ServerError {
                code: 503,
                message: "max clients".to_string(),
            },
        ];
        for err in &errors {
            assert!(!format!("{err}").is_empty());
        }
    }

    #[test]
    fn test_connection_event_variants() {
        let events = vec![
            ConnectionEvent::Connected {
                hostname: "host".to_string(),
            },
            ConnectionEvent::Disconnected,
            ConnectionEvent::ServerError {
                code: 401,
                message: "unauthorized".to_string(),
            },
        ];
        for event in &events {
            assert!(!format!("{event:?}").is_empty());
        }
    }
}

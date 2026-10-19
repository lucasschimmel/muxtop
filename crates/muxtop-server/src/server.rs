use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

/// Timeout for TLS handshake with connecting clients.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const REJECTION_REPORT_INTERVAL: Duration = Duration::from_secs(5);

use muxtop_core::system::SystemSnapshot;
use muxtop_proto::{Frame, WireMessage};

use crate::client;
use crate::rate_limit::RateLimiter;

/// Server configuration derived from CLI arguments.
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub max_clients: usize,
    pub auth_token: String,
    pub refresh_hz: u32,
    pub tls_acceptor: TlsAcceptor,
    /// Per-source-IP token-bucket rate (connections / second). `0.0`
    /// disables rate limiting (every connection is admitted).
    pub rate_limit_per_ip: f32,
}

/// Shared state accessible by all client tasks.
pub struct SharedState {
    pub auth_token: String,
    pub refresh_hz: u32,
    pub hostname: String,
    pub server_version: String,
    pub start_time: Instant,
    pub semaphore: Arc<Semaphore>,
}

/// Run the TCP+TLS server: relay collector snapshots to broadcast, accept clients.
pub async fn run(
    config: ServerConfig,
    collector_rx: mpsc::Receiver<SystemSnapshot>,
    token: CancellationToken,
) -> Result<()> {
    let listener = TcpListener::bind(config.bind).await?;
    run_with_listener(config, listener, collector_rx, token).await
}

/// Run with an already-bound listener, also used by production-path integration tests.
pub async fn run_with_listener(
    config: ServerConfig,
    listener: TcpListener,
    mut collector_rx: mpsc::Receiver<SystemSnapshot>,
    token: CancellationToken,
) -> Result<()> {
    let token = token.child_token();
    tracing::info!(addr = %listener.local_addr()?, "listening (TLS enabled)");

    let tls_acceptor = config.tls_acceptor;

    // PERF-L1 / ADR-30-4: broadcast pre-encoded snapshot frames wrapped in
    // `Arc` so each client task gets a cheap pointer clone instead of a deep
    // `SystemSnapshot` clone (and the bincode encoder runs once per snapshot
    // rather than once per (snapshot, client) pair). The client task type is
    // unchanged at the wire level — it still emits `WireMessage::Snapshot(..)`
    // shaped frames, just from pre-encoded bytes.
    let (broadcast_tx, _) = broadcast::channel::<Arc<Frame>>(16);

    // Shared state for all client tasks.
    let state = Arc::new(SharedState {
        auth_token: config.auth_token,
        refresh_hz: config.refresh_hz,
        hostname: hostname(),
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        start_time: Instant::now(),
        semaphore: Arc::new(Semaphore::new(config.max_clients)),
    });

    // Per-IP rate limiter (burst = refill rate, per ADR-30-3).
    let rate_limiter = Arc::new(RateLimiter::new(
        config.rate_limit_per_ip,
        config.rate_limit_per_ip.max(1.0),
    ));
    if config.rate_limit_per_ip > 0.0 {
        tracing::info!(
            rate = config.rate_limit_per_ip,
            "per-IP rate limiter active (token bucket)"
        );
    } else {
        tracing::warn!("per-IP rate limiter disabled (--rate-limit-per-ip=0)");
    }

    // G-20: Relay task — bridges mpsc (collector) to broadcast (clients).
    //
    // PERF-L1: encode each snapshot ONCE here, then broadcast the resulting
    // `Arc<Frame>` to every subscribed client. Per-client work in the hot
    // path drops from {clone SystemSnapshot, encode bincode} to {clone Arc,
    // write bytes}.
    let relay_tx = broadcast_tx.clone();
    let relay_token = token.clone();
    let relay = tokio::spawn(async move {
        loop {
            tokio::select! {
                snapshot = collector_rx.recv() => {
                    match snapshot {
                        Some(snap) => {
                            let client_count = relay_tx.receiver_count();
                            if client_count == 0 {
                                continue;
                            }
                            match WireMessage::encode_snapshot_ref(&snap) {
                                Ok(frame) => {
                                    let arc_frame = Arc::new(frame);
                                    let _ = relay_tx.send(arc_frame);
                                    tracing::debug!(clients = client_count, "broadcast snapshot");
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "snapshot encode failed; dropping");
                                }
                            }
                        }
                        None => {
                            tracing::debug!("collector channel closed");
                            break;
                        }
                    }
                }
                _ = relay_token.cancelled() => {
                    tracing::debug!("relay task shutting down");
                    break;
                }
            }
        }
    });

    let mut clients = tokio::task::JoinSet::new();
    let mut rejected = 0u64;
    let mut rejection_interval = tokio::time::interval(REJECTION_REPORT_INTERVAL);
    rejection_interval.tick().await;
    let mut accept_error = None;
    // Accept loop.
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (tcp_stream, peer) = match result {
                    Ok(connection) => connection,
                    Err(error) => {
                        accept_error = Some(error);
                        break;
                    }
                };

                // 1. Per-IP rate limiting: drop the TCP stream immediately
                //    (no TLS handshake at all) if the source has exceeded
                //    its token-bucket budget.
                if !rate_limiter.try_admit(peer.ip()) {
                    rejected = rejected.saturating_add(1);
                    drop(tcp_stream);
                    continue;
                }

                // 2. MED-S2: acquire the max_clients permit BEFORE the TLS
                //    handshake so a flood of TLS handshakes cannot saturate
                //    CPU. If we cannot get a permit, drop the TCP stream
                //    without spending any cryptographic effort.
                let permit = match Arc::clone(&state.semaphore).try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        rejected = rejected.saturating_add(1);
                        drop(tcp_stream);
                        continue;
                    }
                };

                tracing::info!(peer = %peer, "client connected, starting TLS handshake");

                let tls_acceptor = tls_acceptor.clone();
                let state = Arc::clone(&state);
                let snapshot_rx = broadcast_tx.subscribe();
                let client_token = token.clone();

                clients.spawn(async move {
                    // TLS handshake (with timeout to prevent slowloris).
                    let tls_stream = match tokio::select! {
                        () = client_token.cancelled() => return,
                        result = tokio::time::timeout(
                        TLS_HANDSHAKE_TIMEOUT,
                        tls_acceptor.accept(tcp_stream),
                    ) => result,
                    } {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            tracing::warn!(peer = %peer, error = %e, "TLS handshake failed");
                            return; // permit dropped here
                        }
                        Err(_) => {
                            tracing::warn!(peer = %peer, "TLS handshake timed out");
                            return; // permit dropped here
                        }
                    };
                    tracing::debug!(peer = %peer, "TLS handshake complete");

                    let (reader, writer) = tokio::io::split(tls_stream);
                    if let Err(e) = client::handle(
                        reader, writer, peer, state, snapshot_rx, client_token, permit,
                    )
                    .await
                    {
                        tracing::debug!(peer = %peer, error = %e, "client session ended");
                    }
                    tracing::info!(peer = %peer, "client disconnected");
                });
            }
            _ = rejection_interval.tick() => {
                if rejected > 0 {
                    tracing::warn!(rejected, "connections rejected by admission limits in the last interval");
                    rejected = 0;
                }
            }
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                if let Err(error) = result {
                    tracing::error!(%error, "client task failed");
                }
            }
            _ = token.cancelled() => {
                tracing::info!("server shutting down, stopping accept loop");
                break;
            }
        }
    }

    token.cancel();
    while let Some(result) = clients.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "client task failed during shutdown");
        }
    }
    if let Err(error) = relay.await {
        tracing::error!(%error, "snapshot relay failed");
    }
    if let Some(error) = accept_error {
        return Err(error.into());
    }
    Ok(())
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxtop_core::network::NetworkSnapshot;
    use muxtop_core::system::{CpuSnapshot, LoadSnapshot, MemorySnapshot, SystemSnapshot};

    #[test]
    fn test_hostname_returns_string() {
        let h = hostname();
        assert!(!h.is_empty());
    }

    #[tokio::test]
    async fn test_server_binds_to_random_port() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert_ne!(addr.port(), 0);
    }

    fn dummy_snapshot() -> SystemSnapshot {
        SystemSnapshot {
            cpu: CpuSnapshot {
                global_usage: 0.0,
                cores: vec![],
            },
            memory: MemorySnapshot {
                total: 0,
                used: 0,
                available: 0,
                swap_total: 0,
                swap_used: 0,
            },
            load: LoadSnapshot {
                one: 0.0,
                five: 0.0,
                fifteen: 0.0,
                uptime_secs: 0,
            },
            processes: vec![],
            networks: NetworkSnapshot {
                interfaces: vec![],
                total_rx: 0,
                total_tx: 0,
            },
            containers: None,
            kube: None,
            gpu: None,
            timestamp_ms: 0,
        }
    }

    /// PERF-L1 / ADR-30-4: every subscribed receiver gets the SAME `Arc<Frame>`
    /// (identity equality), so the bincode encode runs exactly once per
    /// snapshot regardless of how many clients are connected.
    #[tokio::test]
    async fn test_broadcast_arc_frame_shared_across_subscribers() {
        let (tx, _) = broadcast::channel::<Arc<Frame>>(4);
        let mut rx_a = tx.subscribe();
        let mut rx_b = tx.subscribe();
        let mut rx_c = tx.subscribe();

        let snap = dummy_snapshot();
        let frame = WireMessage::encode_snapshot_ref(&snap).unwrap();
        let arc_frame = Arc::new(frame);
        // Pre-send strong count: just our own handle.
        assert_eq!(Arc::strong_count(&arc_frame), 1);
        tx.send(Arc::clone(&arc_frame)).unwrap();

        let a = rx_a.recv().await.unwrap();
        let b = rx_b.recv().await.unwrap();
        let c = rx_c.recv().await.unwrap();

        // Every subscriber received a clone of the same allocation.
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&b, &c));
        assert!(Arc::ptr_eq(&arc_frame, &a));
    }
}

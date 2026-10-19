use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OwnedSemaphorePermit, broadcast};
use tokio::time;
use tokio_util::sync::CancellationToken;

use muxtop_proto::{Frame, FrameReader, FrameWriter, WireMessage, remote::HEARTBEAT_INTERVAL};

use crate::error::ServerError;
use crate::server::SharedState;

/// Bound writes so slow readers cannot retain a session permit indefinitely.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Client must send Hello promptly after the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Pre-authentication payload cap (per ADR-30-2 / MED-S1).
///
/// 4 KiB is generous for `WireMessage::Hello { client_version, auth_token }`
/// (the `client_version` is `env!("CARGO_PKG_VERSION")` and tokens have a
/// 16-char minimum / no maximum, but anything north of 256 chars is already
/// a configuration smell). The post-handshake reads keep the global 4 MiB
/// cap (real snapshots commonly exceed 4 KiB).
pub const PRE_AUTH_HELLO_MAX_PAYLOAD: usize = 4 * 1024;

/// Handle a single client connection: handshake, then stream snapshots + heartbeats.
///
/// Accepts any async reader/writer pair (works with both plain TCP and TLS streams).
///
/// **The caller is responsible for acquiring the `max_clients` semaphore
/// permit BEFORE the TLS handshake** (per MED-S2 / ADR-30-2). The permit
/// is passed in here so it lives for the duration of the session and is
/// released exactly once when this function returns. Acquiring before the
/// handshake prevents a flood of concurrent TLS handshakes from saturating
/// CPU even when all client slots are full.
pub async fn handle<R, W>(
    reader: R,
    writer: W,
    peer: SocketAddr,
    state: Arc<SharedState>,
    mut snapshot_rx: broadcast::Receiver<Arc<Frame>>,
    token: CancellationToken,
    permit: OwnedSemaphorePermit,
) -> Result<(), ServerError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // The permit is held for the entire session and dropped on return.
    let _permit = permit;

    let mut frame_reader = FrameReader::new(reader);
    let mut frame_writer = FrameWriter::new(writer);

    // --- Handshake phase ---
    // Pre-auth read uses a TIGHT 4 KiB cap (MED-S1 / ADR-30-2). The default
    // 4 MiB cap is restored implicitly post-Welcome by switching back to
    // `read_frame()` for snapshot reads (we don't currently read post-Welcome
    // anyway, so this is purely defense-in-depth for future protocol
    // additions).
    let hello_frame = tokio::select! {
        () = token.cancelled() => return Ok(()),
        result = time::timeout(
        HANDSHAKE_TIMEOUT,
        frame_reader.read_frame_with_max_payload(PRE_AUTH_HELLO_MAX_PAYLOAD),
    ) => result,
    }
    .map_err(|_| ServerError::HandshakeTimeout)?
    .map_err(ServerError::Proto)?
    .ok_or(ServerError::HandshakeTimeout)?;

    let hello = WireMessage::from_frame(&hello_frame).map_err(ServerError::Proto)?;

    match &hello {
        WireMessage::Hello {
            client_version,
            auth_token,
        } => {
            tracing::info!(peer = %peer, client_version = %client_version, "received Hello");

            // G-22: Always validate auth token (mandatory).
            let provided = auth_token.as_deref().unwrap_or("");
            if !constant_time_eq(state.auth_token.as_bytes(), provided.as_bytes()) {
                tracing::warn!(peer = %peer, "authentication failed");
                let error_msg = WireMessage::Error {
                    code: 401,
                    message: "unauthorized".into(),
                };
                write_frame(&mut frame_writer, &error_msg.to_frame()?, &token).await?;
                return Err(ServerError::Unauthorized);
            }
        }
        other => {
            return Err(ServerError::UnexpectedMessage {
                expected: "Hello",
                actual: format!("{other:?}"),
            });
        }
    }

    // Send Welcome.
    let welcome = WireMessage::Welcome {
        server_version: state.server_version.clone(),
        hostname: state.hostname.clone(),
        refresh_hz: state.refresh_hz,
    };
    write_frame(&mut frame_writer, &welcome.to_frame()?, &token).await?;

    tracing::info!(peer = %peer, "handshake complete, streaming");

    // --- Streaming phase ---
    let mut heartbeat_interval = time::interval(HEARTBEAT_INTERVAL);
    // Skip the first immediate tick.
    heartbeat_interval.tick().await;

    loop {
        tokio::select! {
            result = snapshot_rx.recv() => {
                match result {
                    Ok(frame) => {
                        // PERF-L1: the relay task already encoded this snapshot
                        // into a `Frame`; we just write the cached bytes.
                        write_frame(&mut frame_writer, &frame, &token).await?;
                        // Reset heartbeat timer after sending a snapshot.
                        heartbeat_interval.reset();
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(peer = %peer, skipped = n, "client lagged, skipping snapshots");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!(peer = %peer, "broadcast closed");
                        break;
                    }
                }
            }
            _ = heartbeat_interval.tick() => {
                let uptime = state.start_time.elapsed().as_secs();
                let msg = WireMessage::Heartbeat {
                    server_version: state.server_version.clone(),
                    uptime_secs: uptime,
                };
                write_frame(&mut frame_writer, &msg.to_frame()?, &token).await?;
                tracing::debug!(peer = %peer, "heartbeat sent");
            }
            _ = token.cancelled() => {
                tracing::debug!(peer = %peer, "shutdown signal received");
                break;
            }
        }
    }

    Ok(())
}

/// Cancellation may leave a partial frame; callers close the session on any error.
async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    frame: &Frame,
    token: &CancellationToken,
) -> Result<(), ServerError> {
    tokio::select! {
        biased;
        () = token.cancelled() => Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "session cancelled").into()),
        result = time::timeout(WRITE_TIMEOUT, writer.write_frame(frame)) => {
            result.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "frame write timed out"))??;
            Ok(())
        }
    }
}

/// Constant-time byte comparison to prevent timing attacks on token validation.
///
/// Both length and content are compared without early returns to avoid
/// leaking the expected token length via timing side-channel.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len_matches = a.len() == b.len();
    let max_len = std::cmp::max(a.len(), b.len());
    let mut diff = 0u8;
    for i in 0..max_len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0 && len_matches
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn blocked_session() -> (
        tokio::task::JoinHandle<Result<(), ServerError>>,
        CancellationToken,
        Arc<tokio::sync::Semaphore>,
        tokio::io::DuplexStream,
    ) {
        let (server, mut peer) = tokio::io::duplex(256);
        let hello = WireMessage::Hello {
            client_version: "test".into(),
            auth_token: Some("placeholder".into()),
        };
        FrameWriter::new(&mut peer)
            .write_frame(&hello.to_frame().unwrap())
            .await
            .unwrap();
        let (reader, writer) = tokio::io::split(server);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let state = Arc::new(SharedState {
            auth_token: "placeholder".into(),
            refresh_hz: 1,
            hostname: "test".into(),
            server_version: "test".into(),
            start_time: std::time::Instant::now(),
            semaphore: semaphore.clone(),
        });
        let (snapshots, rx) = broadcast::channel(1);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(handle(
            reader,
            writer,
            "127.0.0.1:1".parse().unwrap(),
            state,
            rx,
            cancel.clone(),
            permit,
        ));
        let welcome = FrameReader::new(&mut peer)
            .read_frame()
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            WireMessage::from_frame(&welcome).unwrap(),
            WireMessage::Welcome { .. }
        ));
        snapshots
            .send(Arc::new(Frame {
                msg_type: muxtop_proto::MSG_SNAPSHOT,
                payload: vec![0; 4096],
            }))
            .unwrap();
        tokio::task::yield_now().await;
        // Dropping the broadcast sender must not rescue a write blocked on the peer.
        drop(snapshots);
        (task, cancel, semaphore, peer)
    }

    // @generated by:gauntlet run:muxtop-audit-fixes
    #[tokio::test(start_paused = true)]
    async fn t_ac_04_blocked_snapshot_write_times_out_and_releases_permit() {
        let (task, _cancel, semaphore, _peer) = blocked_session().await;
        tokio::time::advance(WRITE_TIMEOUT + Duration::from_secs(1)).await;
        let error = task.await.unwrap().unwrap_err();
        assert!(
            matches!(error, ServerError::Io(ref e) if e.kind() == std::io::ErrorKind::TimedOut)
        );
        assert_eq!(semaphore.available_permits(), 1);
    }

    // @generated by:gauntlet run:muxtop-audit-fixes
    #[tokio::test(start_paused = true)]
    async fn t_ac_04_shutdown_interrupts_blocked_snapshot_write() {
        let (task, cancel, semaphore, _peer) = blocked_session().await;
        cancel.cancel();
        let error = task.await.unwrap().unwrap_err();
        assert!(
            matches!(error, ServerError::Io(ref e) if e.kind() == std::io::ErrorKind::Interrupted)
        );
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"secret", b"secret"));
    }

    #[test]
    fn test_constant_time_eq_different() {
        assert!(!constant_time_eq(b"secret", b"wrong!"));
    }

    #[test]
    fn test_constant_time_eq_different_length() {
        assert!(!constant_time_eq(b"short", b"longer"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_pre_auth_cap_is_4kib() {
        // Sanity guard so we don't accidentally widen the pre-auth cap.
        assert_eq!(PRE_AUTH_HELLO_MAX_PAYLOAD, 4096);
    }
}

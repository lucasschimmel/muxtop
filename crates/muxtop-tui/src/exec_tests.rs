// Acceptance trace: T-AC-04, T-AC-05, T-AC-06, T-AC-07 (test names use Rust-compatible t_ac_ identifiers).
// @generated run:muxtop-exec-pty — spec-first behavior tests.
use super::*;
use futures::stream;
use std::sync::{Arc, atomic::Ordering};

#[path = "exec_test_support.rs"]
mod support;
use support::*;
#[path = "exec_app_tests.rs"]
mod app;

#[test]
fn t_ac_04_detach_consumes_escape_and_typeahead_at_every_chunk_boundary() {
    let bytes = b"\x00\xff\x03\x04\x1b[A\r\n";
    for position in 0..=bytes.len() {
        let mut input = bytes[..position].to_vec();
        input.push(DETACH);
        input.extend_from_slice(b"qkill\n");
        assert_eq!(before_detach(&input), (&bytes[..position], true));
        assert_eq!(
            before_detach(&bytes[..position]),
            (&bytes[..position], false)
        );
    }
}

#[tokio::test]
async fn t_ac_04_raw_controls_forward_unchanged_until_detach() {
    let remote = Writer::default();
    let observed = remote.bytes.clone();
    let bytes = b"\x00\xff\x03\x04\x1b[A\r\n\x1dquit\n";
    let result = bridge(
        session(remote, stream::pending(), Arc::default()),
        &bytes[..],
        tokio::io::sink(),
        stream::pending(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(result.unwrap(), Outcome::Detached);
    assert_eq!(*observed.lock().unwrap(), b"\x00\xff\x03\x04\x1b[A\r\n");
}

#[tokio::test]
async fn t_ac_04_output_is_binary_safe_and_shell_status_is_preserved() {
    for code in [Some(0), Some(127), None] {
        let (_held, input) = pending_input();
        let output = Writer::default();
        let observed = output.bytes.clone();
        let control = Arc::new(Control {
            code,
            ..Default::default()
        });
        let result = bridge(
            session(
                Writer::default(),
                stream::iter([Ok(vec![0, 255, 27]), Ok(b"[31m\r\n".to_vec())]),
                control,
            ),
            input,
            output,
            stream::pending(),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(result.unwrap(), Outcome::Exited(code));
        assert_eq!(*observed.lock().unwrap(), b"\x00\xff\x1b[31m\r\n");
    }
}

#[tokio::test]
async fn t_ac_06_local_eof_closes_remote_input() {
    let remote = Writer::default();
    let drops = remote.drops.clone();
    let result = bridge(
        session(remote, stream::pending(), Arc::default()),
        tokio::io::empty(),
        tokio::io::sink(),
        stream::pending(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(result.unwrap(), Outcome::Detached);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn t_ac_06_daemon_output_error_closes_remote_input() {
    let (_held, input) = pending_input();
    let remote = Writer::default();
    let drops = remote.drops.clone();
    let result = bridge(
        session(
            remote,
            stream::iter([Err(EngineError::Other("transport failed".into()))]),
            Arc::default(),
        ),
        input,
        tokio::io::sink(),
        stream::pending(),
        CancellationToken::new(),
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("transport failed"));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn t_ac_06_local_output_error_and_remote_input_error_are_reported() {
    let (_held, input) = pending_input();
    let result = bridge(
        session(
            Writer::default(),
            stream::iter([Ok(vec![1])]),
            Arc::default(),
        ),
        input,
        Writer::failing(),
        stream::pending(),
        CancellationToken::new(),
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("PTY I/O failed"));
    let result = bridge(
        session(Writer::failing(), stream::pending(), Arc::default()),
        &b"input"[..],
        tokio::io::sink(),
        stream::pending(),
        CancellationToken::new(),
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("PTY I/O failed"));
}

#[tokio::test(start_paused = true)]
async fn t_ac_06_stalled_writes_have_a_deadline_in_both_directions() {
    let result = bridge(
        session(Writer::stalled(), stream::pending(), Arc::default()),
        &b"input"[..],
        tokio::io::sink(),
        stream::pending(),
        CancellationToken::new(),
    )
    .await;
    assert!(matches!(result, Err(EngineError::Timeout(duration)) if duration == IO_TIMEOUT));
    let (_held, input) = pending_input();
    let result = bridge(
        session(
            Writer::default(),
            stream::iter([Ok(vec![1])]),
            Arc::default(),
        ),
        input,
        Writer::stalled(),
        stream::pending(),
        CancellationToken::new(),
    )
    .await;
    assert!(matches!(result, Err(EngineError::Timeout(duration)) if duration == IO_TIMEOUT));
}

#[tokio::test]
async fn t_ac_06_cancellation_interrupts_a_stalled_write_and_releases_transport() {
    let remote = Writer::stalled();
    let drops = remote.drops.clone();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(bridge(
        session(remote, stream::pending(), Arc::default()),
        &b"input"[..],
        tokio::io::sink(),
        stream::pending(),
        cancel.clone(),
    ));
    tokio::task::yield_now().await;
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), Outcome::Detached);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn t_ac_06_dropping_bridge_releases_pending_transport() {
    let remote = Writer::default();
    let drops = remote.drops.clone();
    let (_held, input) = pending_input();
    let task = tokio::spawn(bridge(
        session(remote, stream::pending(), Arc::default()),
        input,
        tokio::io::sink(),
        stream::pending(),
        CancellationToken::new(),
    ));
    tokio::task::yield_now().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn t_ac_05_resize_preserves_dimensions_and_propagates_daemon_errors() {
    let (_held, input) = pending_input();
    let control = Arc::new(Control {
        resize_fails: true,
        ..Default::default()
    });
    let size = ExecSize {
        columns: 137,
        rows: 43,
    };
    let result = bridge(
        session(Writer::default(), stream::pending(), control.clone()),
        input,
        tokio::io::sink(),
        stream::iter([size]),
        CancellationToken::new(),
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("resize denied"));
    assert_eq!(*control.sizes.lock().unwrap(), vec![size]);
}

#[tokio::test]
async fn t_ac_05_resize_updates_the_existing_session_without_ending_io() {
    let (mut local, input) = pending_input();
    let control = Arc::new(Control::default());
    let sizes = vec![
        ExecSize {
            columns: 80,
            rows: 24,
        },
        ExecSize {
            columns: 140,
            rows: 50,
        },
    ];
    let task = tokio::spawn(bridge(
        session(Writer::default(), stream::pending(), control.clone()),
        input,
        tokio::io::sink(),
        stream::iter(sizes.clone()),
        CancellationToken::new(),
    ));
    tokio::task::yield_now().await;
    assert_eq!(*control.sizes.lock().unwrap(), sizes);
    assert!(!task.is_finished());
    local.write_all(&[DETACH]).await.unwrap();
    assert_eq!(task.await.unwrap().unwrap(), Outcome::Detached);
}

#[tokio::test]
async fn t_ac_07_exit_inspection_error_is_reported() {
    let (_held, input) = pending_input();
    let control = Arc::new(Control {
        inspect_fails: true,
        ..Default::default()
    });
    let result = bridge(
        session(Writer::default(), stream::empty(), control),
        input,
        tokio::io::sink(),
        stream::pending(),
        CancellationToken::new(),
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("inspect denied"));
}

#[tokio::test]
async fn t_ac_04_live_duplex_session_forwards_both_directions() {
    let (mut local, input) = pending_input();
    let (remote_input, mut daemon_input) = tokio::io::duplex(64);
    let (daemon_output, remote_output) = futures::channel::mpsc::unbounded();
    let output = Writer::default();
    let observed = output.bytes.clone();
    let session = ExecSession {
        input: Box::pin(remote_input),
        output: Box::pin(remote_output),
        control: Arc::new(Control {
            code: Some(0),
            ..Default::default()
        }),
    };
    let task = tokio::spawn(bridge(
        session,
        input,
        output,
        stream::pending(),
        CancellationToken::new(),
    ));

    local.write_all(b"echo hi\r\x03\x04").await.unwrap();
    let mut bytes = [0; 10];
    daemon_input.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"echo hi\r\x03\x04");
    daemon_output
        .unbounded_send(Ok(b"hi\r\n".to_vec()))
        .unwrap();
    drop(daemon_output);

    assert_eq!(task.await.unwrap().unwrap(), Outcome::Exited(Some(0)));
    assert_eq!(*observed.lock().unwrap(), b"hi\r\n");
}

#[tokio::test]
async fn t_ac_06_cancellation_interrupts_output_backpressure_with_idle_input() {
    let (_held, input) = pending_input();
    let remote = Writer::default();
    let drops = remote.drops.clone();
    let cancel = CancellationToken::new();
    let output = Writer::stalled();
    let task = tokio::spawn(bridge(
        session(remote, stream::iter([Ok(vec![1])]), Arc::default()),
        input,
        output,
        stream::pending(),
        cancel.clone(),
    ));
    tokio::task::yield_now().await;
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), Outcome::Detached);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn t_ac_07_exit_inspection_has_a_deadline() {
    let (_held, input) = pending_input();
    let control = Arc::new(Control {
        inspect_stalls: true,
        ..Default::default()
    });
    let result = bridge(
        session(Writer::default(), stream::empty(), control),
        input,
        tokio::io::sink(),
        stream::pending(),
        CancellationToken::new(),
    )
    .await;
    assert!(matches!(result, Err(EngineError::Timeout(duration)) if duration == IO_TIMEOUT));
}

#[tokio::test]
async fn t_ac_06_cancellation_releases_transport_before_pending_exit_inspection() {
    let (_held, input) = pending_input();
    let remote = Writer::default();
    let drops = remote.drops.clone();
    let control = Arc::new(Control {
        inspect_stalls: true,
        ..Default::default()
    });
    let cancel = CancellationToken::new();
    let task = tokio::spawn(bridge(
        session(remote, stream::empty(), control.clone()),
        input,
        tokio::io::sink(),
        stream::pending(),
        cancel.clone(),
    ));
    tokio::task::yield_now().await;
    assert_eq!(control.inspect_started.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(control.output_drops.load(Ordering::SeqCst), 1);
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), Outcome::Detached);
}

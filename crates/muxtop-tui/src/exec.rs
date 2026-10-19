//! A PTY owns terminal input exclusively until EOF, detach or error.

use futures::{Stream, StreamExt};
use muxtop_core::container_engine::EngineError;
use muxtop_core::container_exec::{ExecSession, ExecSize};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(10);
const BUFFER_SIZE: usize = 4096;
const DETACH: u8 = 0x1d; // Ctrl+]

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Exited(Option<i64>),
    Detached,
}

/// Bytes before the local escape are forwarded; the escape and typeahead after
/// it are consumed, so they cannot accidentally activate actions in the TUI.
fn before_detach(bytes: &[u8]) -> (&[u8], bool) {
    match bytes.iter().position(|byte| *byte == DETACH) {
        Some(index) => (&bytes[..index], true),
        None => (bytes, false),
    }
}

fn io_error(error: std::io::Error) -> EngineError {
    EngineError::Other(format!("PTY I/O failed: {error}"))
}

async fn forward_input(
    mut local: impl AsyncRead + Unpin,
    mut remote: std::pin::Pin<Box<dyn AsyncWrite + Send>>,
) -> Result<Outcome, EngineError> {
    let mut buffer = [0; BUFFER_SIZE];
    loop {
        let length = local.read(&mut buffer).await.map_err(io_error)?;
        if length == 0 {
            return Ok(Outcome::Detached);
        }
        let (bytes, detach) = before_detach(&buffer[..length]);
        timeout_io(remote.write_all(bytes)).await?;
        if detach {
            return Ok(Outcome::Detached);
        }
    }
}

async fn timeout_io<T>(
    future: impl std::future::Future<Output = std::io::Result<T>>,
) -> Result<T, EngineError> {
    tokio::time::timeout(IO_TIMEOUT, future)
        .await
        .map_err(|_| EngineError::Timeout(IO_TIMEOUT))?
        .map_err(io_error)
}

async fn forward_output(
    mut remote: std::pin::Pin<Box<dyn Stream<Item = Result<Vec<u8>, EngineError>> + Send>>,
    mut local: impl AsyncWrite + Unpin,
) -> Result<(), EngineError> {
    while let Some(chunk) = remote.next().await {
        timeout_io(local.write_all(&chunk?)).await?;
        timeout_io(local.flush()).await?;
    }
    Ok(())
}

/// No input threads or detached tasks: dropping a select branch drops all its
/// I/O futures and the transport halves. Each stalled write has a deadline.
#[cfg(test)]
pub(crate) async fn bridge(
    session: ExecSession,
    input: impl AsyncRead + Unpin,
    output: impl AsyncWrite + Unpin,
    sizes: impl Stream<Item = ExecSize> + Unpin,
    cancel: CancellationToken,
) -> Result<Outcome, EngineError> {
    bridge_checked(session, input, output, sizes.map(Ok), cancel).await
}

async fn bridge_checked(
    session: ExecSession,
    input: impl AsyncRead + Unpin,
    output: impl AsyncWrite + Unpin,
    sizes: impl Stream<Item = Result<ExecSize, EngineError>> + Unpin,
    cancel: CancellationToken,
) -> Result<Outcome, EngineError> {
    let ExecSession {
        input: remote_in,
        output: remote_out,
        control,
    } = session;
    let resize_control = control.clone();
    let resize = async move {
        let mut sizes = sizes;
        while let Some(size) = sizes.next().await {
            resize_control.resize(size?).await?;
        }
        std::future::pending::<Result<(), EngineError>>().await
    };
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => Some(Outcome::Detached),
        result = forward_output(remote_out, output) => { result?; None }
        result = forward_input(input, remote_in) => Some(result?),
        result = resize => { result?; unreachable!("resize loop does not complete successfully") }
    };
    // The I/O futures (and both transport halves) have been dropped before we
    // inspect the process. Cancellation also interrupts a stalled inspect.
    if let Some(outcome) = outcome {
        return Ok(outcome);
    }
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Ok(Outcome::Detached),
        result = tokio::time::timeout(IO_TIMEOUT, control.exit_code()) => {
            Ok(Outcome::Exited(result.map_err(|_| EngineError::Timeout(IO_TIMEOUT))??))
        }
    }
}

#[cfg(unix)]
pub(crate) fn run(
    engine: &dyn muxtop_core::container_engine::ContainerEngine,
    id: &str,
    cancel: CancellationToken,
) -> Result<Outcome, EngineError> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(EngineError::Other(
            "interactive exec requires a terminal on stdin and stdout".into(),
        ));
    }
    tokio::runtime::Handle::current().block_on(async {
        let mut tty = crate::exec_tty::NativeTty::open().map_err(io_error)?;
        timeout_io(
            tty.write_all(b"\r\nmuxtop container shell: Ctrl+] to detach; exit to return\r\n"),
        )
        .await?;
        let size = terminal_size()?;
        let session = engine.exec_shell(id, size).await?;
        let sizes = futures::stream::unfold(size, |mut previous| async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                match terminal_size() {
                    Ok(size) if size != previous => {
                        previous = size;
                        return Some((Ok(size), previous));
                    }
                    Ok(_) => {}
                    Err(error) => return Some((Err(error), previous)),
                }
            }
        });
        bridge_checked(session, tty.clone(), tty, Box::pin(sizes), cancel).await
    })
}

#[cfg(unix)]
fn terminal_size() -> Result<ExecSize, EngineError> {
    let (columns, rows) = crossterm::terminal::size().map_err(io_error)?;
    Ok(ExecSize {
        columns: columns.max(1),
        rows: rows.max(1),
    })
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;

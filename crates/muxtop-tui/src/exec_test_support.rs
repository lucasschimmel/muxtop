// @generated run:muxtop-exec-pty — spec-first behavior tests.
use super::*;
use async_trait::async_trait;
use muxtop_core::container_exec::ExecControl;
use std::{
    pin::Pin,
    sync::{Mutex, atomic::AtomicUsize},
    task::{Context, Poll},
};

#[derive(Default)]
pub(super) struct Control {
    pub(super) sizes: Mutex<Vec<ExecSize>>,
    pub(super) code: Option<i64>,
    pub(super) resize_fails: bool,
    pub(super) inspect_fails: bool,
    pub(super) inspect_stalls: bool,
    pub(super) inspect_started: AtomicUsize,
    pub(super) output_drops: Arc<AtomicUsize>,
}
#[async_trait]
impl ExecControl for Control {
    async fn resize(&self, size: ExecSize) -> Result<(), EngineError> {
        self.sizes.lock().unwrap().push(size);
        if self.resize_fails {
            return Err(EngineError::Other("resize denied".into()));
        }
        Ok(())
    }
    async fn exit_code(&self) -> Result<Option<i64>, EngineError> {
        self.inspect_started.fetch_add(1, Ordering::SeqCst);
        if self.inspect_stalls {
            std::future::pending::<()>().await;
        }
        if self.inspect_fails {
            return Err(EngineError::Other("inspect denied".into()));
        }
        Ok(self.code)
    }
}

#[derive(Clone, Default)]
pub(super) struct Writer {
    pub(super) bytes: Arc<Mutex<Vec<u8>>>,
    pub(super) drops: Arc<AtomicUsize>,
    fails: bool,
    stalls: bool,
}
impl Writer {
    pub(super) fn failing() -> Self {
        let mut writer = Self::default();
        writer.fails = true;
        writer
    }
    pub(super) fn stalled() -> Self {
        let mut writer = Self::default();
        writer.stalls = true;
        writer
    }
}
impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.stalls {
            return Poll::Pending;
        }
        if self.fails {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct TrackedOutput {
    stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>, EngineError>> + Send>>,
    drops: Arc<AtomicUsize>,
}
impl Stream for TrackedOutput {
    type Item = Result<Vec<u8>, EngineError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}
impl Drop for TrackedOutput {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
pub(super) fn session(
    writer: Writer,
    chunks: impl Stream<Item = Result<Vec<u8>, EngineError>> + Send + 'static,
    control: Arc<Control>,
) -> ExecSession {
    ExecSession {
        input: Box::pin(writer),
        output: Box::pin(TrackedOutput {
            stream: Box::pin(chunks),
            drops: control.output_drops.clone(),
        }),
        control,
    }
}
pub(super) fn pending_input() -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
    tokio::io::duplex(64)
}

//! Local interactive exec transport; deliberately absent from the snapshot/wire model.

use crate::container_engine::EngineError;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::AsyncWrite;

/// Character dimensions, ordered explicitly to avoid swapping Docker's height/width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecSize {
    pub columns: u16,
    pub rows: u16,
}

/// Control requests use the same daemon and exec ID as the attached byte stream.
#[async_trait]
pub trait ExecControl: Send + Sync {
    async fn resize(&self, size: ExecSize) -> Result<(), EngineError>;
    /// `None` means the daemon cannot yet report a completed process's exit code.
    async fn exit_code(&self) -> Result<Option<i64>, EngineError>;
}

/// Owned halves of an attached daemon-side PTY. Dropping both closes the transport;
/// it does not guarantee termination of the process inside the container.
pub struct ExecSession {
    pub output: Pin<Box<dyn Stream<Item = Result<Vec<u8>, EngineError>> + Send>>,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
    pub control: Arc<dyn ExecControl>,
}

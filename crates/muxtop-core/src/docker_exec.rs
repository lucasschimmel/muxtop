//! Bollard exec adapter. No CLI subprocess, local shell interpolation or elevated user.

use crate::container_engine::{ConnectionTarget, EngineError};
use crate::container_exec::{ExecControl, ExecSession, ExecSize};
use crate::docker_engine::map_bollard_error;
use async_trait::async_trait;
use bollard::Docker;
use bollard::models::ExecConfig;
use bollard::query_parameters::ResizeExecOptionsBuilder;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

struct DockerExecControl {
    docker: Docker,
    id: String,
}

#[async_trait]
impl ExecControl for DockerExecControl {
    async fn resize(&self, size: ExecSize) -> Result<(), EngineError> {
        let options = ResizeExecOptionsBuilder::default()
            .w(i32::from(size.columns.max(1)))
            .h(i32::from(size.rows.max(1)))
            .build();
        timeout(CONTROL_TIMEOUT, self.docker.resize_exec(&self.id, options))
            .await
            .map_err(|_| EngineError::Timeout(CONTROL_TIMEOUT))?
            .map_err(map_bollard_error)
    }

    async fn exit_code(&self) -> Result<Option<i64>, EngineError> {
        let state = timeout(CONTROL_TIMEOUT, self.docker.inspect_exec(&self.id))
            .await
            .map_err(|_| EngineError::Timeout(CONTROL_TIMEOUT))?
            .map_err(map_bollard_error)?;
        Ok(if state.running == Some(false) {
            state.exit_code
        } else {
            None
        })
    }
}

pub(crate) async fn open_shell(
    docker: Docker,
    target: &ConnectionTarget,
    id: &str,
    size: ExecSize,
) -> Result<ExecSession, EngineError> {
    // One deadline covers create + start + initial resize. Never retry start:
    // a lost response can still mean a process was created on the daemon.
    timeout(CONTROL_TIMEOUT, async {
        let created = docker
            .create_exec(
                id,
                ExecConfig {
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(true),
                    cmd: Some(vec!["/bin/sh".into()]),
                    env: Some(vec![format!(
                        "TERM={}",
                        std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into())
                    )]),
                    console_size: Some(vec![
                        usize::from(size.rows.max(1)),
                        usize::from(size.columns.max(1)),
                    ]),
                    ..Default::default()
                },
            )
            .await
            .map_err(map_bollard_error)?;
        let (output, input) = crate::docker_exec_transport::attach(target, &created.id).await?;
        let control = Arc::new(DockerExecControl {
            docker,
            id: created.id,
        });
        control.resize(size).await?;
        Ok(ExecSession {
            output,
            input,
            control,
        })
    })
    .await
    .map_err(|_| EngineError::Timeout(CONTROL_TIMEOUT))?
}

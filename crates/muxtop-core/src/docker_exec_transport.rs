//! Raw HTTP upgrade for exec PTYs. Bollard 0.21's start_exec applies a log-frame
//! decoder even with Tty=true: leading 0x00..0x02 bytes are mistaken for a frame
//! header. A PTY has no multiplexing headers, so preserve every byte instead.

use crate::container_engine::{ConnectionTarget, EngineError};
use bytes::Bytes;
use futures::Stream;
use http::{Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

type Output = Pin<Box<dyn Stream<Item = Result<Vec<u8>, EngineError>> + Send>>;
type Input = Pin<Box<dyn AsyncWrite + Send>>;
trait Transport: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Transport for T {}

/// Abort the HTTP driver on setup failure/timeout. On success it has completed
/// transferring ownership to Upgraded before this guard drops.
struct Driver(tokio::task::AbortHandle);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn transport_error(error: impl std::fmt::Display) -> EngineError {
    EngineError::Other(format!("exec transport: {error}"))
}

async fn connect(target: &ConnectionTarget) -> Result<(Box<dyn Transport>, String), EngineError> {
    match target {
        ConnectionTarget::Unix(path) => {
            #[cfg(unix)]
            {
                Ok((
                    Box::new(
                        tokio::net::UnixStream::connect(path)
                            .await
                            .map_err(transport_error)?,
                    ),
                    "localhost".into(),
                ))
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                Err(EngineError::Other("Unix exec sockets require Unix".into()))
            }
        }
        ConnectionTarget::Http(url) => {
            let uri: Uri = url.parse().map_err(transport_error)?;
            if uri.scheme_str() != Some("http") {
                return Err(EngineError::Other(
                    "exec requires the engine's HTTP or Unix socket endpoint".into(),
                ));
            }
            let host = uri
                .host()
                .ok_or_else(|| EngineError::Other("engine endpoint has no host".into()))?;
            let port = uri.port_u16().unwrap_or(80); // HTTP's standard default.
            let socket = tokio::net::TcpStream::connect((host.trim_matches(['[', ']']), port))
                .await
                .map_err(transport_error)?;
            let authority = uri
                .authority()
                .ok_or_else(|| EngineError::Other("engine endpoint has no authority".into()))?
                .to_string();
            Ok((Box::new(socket), authority))
        }
    }
}

pub(crate) async fn attach(
    target: &ConnectionTarget,
    id: &str,
) -> Result<(Output, Input), EngineError> {
    let (socket, host) = connect(target).await?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .map_err(transport_error)?;
    let task = tokio::spawn(connection.with_upgrades());
    let _driver = Driver(task.abort_handle());
    // Match Bollard 0.21's actual unversioned request routes (its absolute
    // path URL join discards both base paths and API-version prefixes).
    let request = Request::post(format!("/exec/{id}/start"))
        .header(http::header::HOST, host)
        .header(http::header::CONNECTION, "Upgrade")
        .header(http::header::UPGRADE, "tcp")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from_static(
            br#"{"Detach":false,"Tty":true}"#,
        )))
        .map_err(transport_error)?;
    let response = sender
        .send_request(request)
        .await
        .map_err(transport_error)?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(response_error(response).await);
    }
    let upgraded = hyper::upgrade::on(response)
        .await
        .map_err(transport_error)?;
    task.await
        .map_err(transport_error)?
        .map_err(transport_error)?;
    let (read, write) = tokio::io::split(TokioIo::new(upgraded));
    let output = futures::stream::unfold(Some(read), |read| async move {
        let mut read = read?;
        let mut buffer = vec![0; 4096];
        match read.read(&mut buffer).await {
            Ok(0) => None,
            Ok(length) => {
                buffer.truncate(length);
                Some((Ok(buffer), Some(read)))
            }
            Err(error) => Some((Err(transport_error(error)), None)),
        }
    });
    Ok((Box::pin(output), Box::pin(write)))
}

async fn response_error(response: hyper::Response<hyper::body::Incoming>) -> EngineError {
    let status = response.status();
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    const MAX_ERROR_BYTES: usize = 16 * 1024;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            break;
        };
        if let Ok(data) = frame.into_data() {
            let remaining = MAX_ERROR_BYTES - bytes.len();
            bytes.extend_from_slice(&data[..data.len().min(remaining)]);
            if bytes.len() == MAX_ERROR_BYTES {
                break;
            }
        }
    }
    let message = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(|message| message.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("daemon refused exec start (HTTP {status})"));
    match status {
        StatusCode::FORBIDDEN => EngineError::PermissionDenied(message),
        StatusCode::NOT_FOUND => EngineError::ContainerNotFound(message),
        _ => EngineError::Other(message),
    }
}

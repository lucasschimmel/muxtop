use std::{future::Future, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

pub(crate) struct Server {
    pub url: String,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn serve<F, Fut>(handler: F) -> Server
where
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = (u16, String)> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let url = format!("http://{}", listener.local_addr().expect("fixture address"));
    let handler = Arc::new(handler);
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let count = stream.read(&mut buf).await.expect("read fixture request");
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..count]);
                }
                let request = String::from_utf8(request).expect("HTTP request");
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .expect("request path")
                    .to_string();
                let (status, body) = handler(path).await;
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    Server { url, task }
}

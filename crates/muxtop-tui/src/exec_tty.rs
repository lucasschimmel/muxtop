//! Read/write the controlling terminal through a separate nonblocking open file
//! description. No blocking stdin worker and no changes to inherited fd flags.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

#[derive(Clone)]
pub(crate) struct NativeTty(Arc<AsyncFd<File>>);

impl NativeTty {
    pub(crate) fn open() -> io::Result<Self> {
        // Darwin's kqueue rejects the /dev/tty alias with EINVAL. Register the
        // real terminal device instead; opening it separately still avoids
        // changing O_NONBLOCK on muxtop's inherited stdin/stdout descriptors.
        let mut name = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: stdin is live; name is writable for exactly name.len() bytes.
        let error =
            unsafe { libc::ttyname_r(libc::STDIN_FILENO, name.as_mut_ptr().cast(), name.len()) };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        let length = name
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| io::Error::other("terminal device path is not terminated"))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(std::ffi::OsStr::from_bytes(&name[..length]))?;
        Ok(Self(Arc::new(AsyncFd::new(file)?)))
    }
}

impl AsyncRead for NativeTty {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;
            match guard.try_io(|inner| {
                let mut file = inner.get_ref();
                file.read(buffer.initialize_unfilled())
            }) {
                Ok(Ok(length)) => {
                    buffer.advance(length);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_) => continue,
            }
        }
    }
}

impl AsyncWrite for NativeTty {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.0.poll_write_ready(cx))?;
            match guard.try_io(|inner| {
                let mut file = inner.get_ref();
                file.write(buffer)
            }) {
                Ok(result) => return Poll::Ready(result),
                Err(_) => continue,
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
#[path = "exec_tty_tests.rs"]
mod tests;

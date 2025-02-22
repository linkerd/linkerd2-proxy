use crate::{ClientAddr, Keepalive, Local, Remote, ServerAddr, UserTimeout};
use linkerd_io as io;
use linkerd_stack::{Param, Service};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::debug;

#[derive(Copy, Clone, Debug)]
pub struct ConnectTcp {
    keepalive: Keepalive,
    user_timeout: UserTimeout,
}

impl ConnectTcp {
    pub fn new(keepalive: Keepalive, user_timeout: UserTimeout) -> Self {
        Self {
            keepalive,
            user_timeout,
        }
    }
}

impl<T: Param<Remote<ServerAddr>>> Service<T> for ConnectTcp {
    type Response = (io::ScopedIo<self::net::TcpStream>, Local<ClientAddr>);
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send + Sync + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let Keepalive(keepalive) = self.keepalive;
        let UserTimeout(user_timeout) = self.user_timeout;
        let Remote(ServerAddr(addr)) = t.param();
        debug!(server.addr = %addr, "Connecting");
        Box::pin(async move {
            let io = tokio::net::TcpStream::connect(&addr).await?;
            super::set_nodelay_or_warn(&io);
            let io = super::set_keepalive_or_warn(io, keepalive)?;
            let io = super::set_user_timeout_or_warn(io, user_timeout)?;
            let local_addr = io.local_addr()?;
            debug!(
                local.addr = %local_addr,
                ?keepalive,
                "Connected",
            );
            Ok((
                io::ScopedIo::client(self::net::TcpStream(io)),
                Local(ClientAddr(local_addr)),
            ))
        })
    }
}

mod net {
    use super::*;

    /// A wrapper that implements Tokio's IO traits for an inner type that
    /// implements hyper's IO traits, or vice versa (implements hyper's IO
    /// traits for a type that implements Tokio's IO traits).
    #[derive(Debug)]
    pub struct TcpStream(pub tokio::net::TcpStream);

    impl TcpStream {
        fn project(self: Pin<&mut Self>) -> Pin<&mut tokio::net::TcpStream> {
            let Self(stream) = self.get_mut();
            Pin::new(stream)
        }
    }

    impl tokio::io::AsyncRead for TcpStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut linkerd_io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            self.project().poll_read(cx, buf)
        }
    }

    impl tokio::io::AsyncWrite for TcpStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, std::io::Error>> {
            self.project().poll_write(cx, buf)
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            self.project().poll_flush(cx)
        }
        fn poll_shutdown(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            self.project().poll_shutdown(cx)
        }
        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[std::io::IoSlice<'_>],
        ) -> Poll<Result<usize, std::io::Error>> {
            self.project().poll_write_vectored(cx, bufs)
        }
        fn is_write_vectored(&self) -> bool {
            self.0.is_write_vectored()
        }
    }

    impl hyper::rt::Read for TcpStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            mut buf: hyper::rt::ReadBufCursor<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            let n = unsafe {
                let mut tbuf = tokio::io::ReadBuf::uninit(buf.as_mut());
                match tokio::io::AsyncRead::poll_read(self.project(), cx, &mut tbuf) {
                    Poll::Ready(Ok(())) => tbuf.filled().len(),
                    other => return other,
                }
            };

            unsafe {
                buf.advance(n);
            }
            Poll::Ready(Ok(()))
        }
    }

    impl hyper::rt::Write for TcpStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, std::io::Error>> {
            tokio::io::AsyncWrite::poll_write(self.project(), cx, buf)
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            tokio::io::AsyncWrite::poll_flush(self.project(), cx)
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            tokio::io::AsyncWrite::poll_shutdown(self.project(), cx)
        }

        fn is_write_vectored(&self) -> bool {
            tokio::io::AsyncWrite::is_write_vectored(&self.0)
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[std::io::IoSlice<'_>],
        ) -> Poll<Result<usize, std::io::Error>> {
            tokio::io::AsyncWrite::poll_write_vectored(self.project(), cx, bufs)
        }
    }
}

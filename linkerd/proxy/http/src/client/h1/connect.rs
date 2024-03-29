use futures::TryFuture;
use hyper::client::connect as hyper_connect;
use linkerd_error::{Error, Result};
use linkerd_io::{self as io, AsyncRead, AsyncWrite};
use linkerd_stack::{MakeConnection, Service};
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Glue for any `tokio_connect::Connect` to implement `hyper::client::Connect`.
#[derive(Debug, Clone)]
pub struct Connect<C, T> {
    connect: C,
    absolute_form: bool,
    target: T,
}

#[pin_project]
#[derive(Debug, Clone)]
pub struct Connection<T> {
    #[pin]
    transport: T,
    absolute_form: bool,
}

/// Future returned by `Connect`.
#[pin_project]
pub struct ConnectFuture<F> {
    #[pin]
    inner: F,
    absolute_form: bool,
}

// === impl Connect ===

impl<C, T> Connect<C, T> {
    pub(super) fn new(connect: C, target: T, absolute_form: bool) -> Self {
        Connect {
            connect,
            target,
            absolute_form,
        }
    }
}

impl<C, T> Service<hyper::Uri> for Connect<C, T>
where
    C: MakeConnection<(crate::Version, T)> + Clone + Send + Sync,
    C::Connection: Unpin + Send,
    C::Future: Unpin + Send + 'static,
    T: Clone + Send + Sync,
{
    type Response = Connection<C::Connection>;
    type Error = Error;
    type Future = ConnectFuture<C::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connect.poll_ready(cx).map_err(Into::into)
    }

    #[inline]
    fn call(&mut self, _dst: hyper::Uri) -> Self::Future {
        ConnectFuture {
            inner: self
                .connect
                .connect((crate::Version::Http1, self.target.clone())),
            absolute_form: self.absolute_form,
        }
    }
}

impl<F, I, M> Future for ConnectFuture<F>
where
    F: TryFuture<Ok = (I, M)> + 'static,
    F::Error: Into<Error>,
{
    type Output = Result<Connection<I>>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let (transport, _) = futures::ready!(this.inner.try_poll(cx)).map_err(Into::into)?;
        Poll::Ready(Ok(Connection {
            transport,
            absolute_form: *this.absolute_form,
        }))
    }
}

// === impl Connected ===

impl<C> AsyncRead for Connection<C>
where
    C: AsyncRead,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.project().transport.poll_read(cx, buf)
    }
}

impl<C> AsyncWrite for Connection<C>
where
    C: AsyncWrite,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.project().transport.poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        self.project().transport.poll_write_vectored(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.project().transport.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.project().transport.poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.transport.is_write_vectored()
    }
}

impl<C> hyper_connect::Connection for Connection<C> {
    fn connected(&self) -> hyper_connect::Connected {
        hyper_connect::Connected::new().proxy(self.absolute_form)
    }
}

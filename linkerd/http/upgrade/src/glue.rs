use crate::upgrade::Http11Upgrade;
use futures::{ready, TryFuture};
use http_body::Body;
use hyper::client::connect as hyper_connect;
use linkerd_error::{Error, Result};
use linkerd_http_box::BoxBody;
use linkerd_io::{self as io, AsyncRead, AsyncWrite};
use linkerd_stack::{MakeConnection, Service};
use pin_project::{pin_project, pinned_drop};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::debug;

/// Provides optional HTTP/1.1 upgrade support on the body.
#[pin_project(PinnedDrop)]
#[derive(Debug)]
pub struct UpgradeBody<B = BoxBody> {
    /// The inner [`Body`] being wrapped.
    #[pin]
    body: B,
    upgrade: Option<(Http11Upgrade, hyper::upgrade::OnUpgrade)>,
}

/// Glue for any `tokio_connect::Connect` to implement `hyper::client::Connect`.
#[derive(Debug, Clone)]
pub struct HyperConnect<C, T> {
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

/// Future returned by `HyperConnect`.
#[pin_project]
pub struct HyperConnectFuture<F> {
    #[pin]
    inner: F,
    absolute_form: bool,
}

// === impl UpgradeBody ===

impl<B> Body for UpgradeBody<B>
where
    B: Body,
    B::Error: std::fmt::Display,
{
    type Data = B::Data;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        // Poll the next chunk from the body.
        let this = self.project();
        let body = this.body;
        let data = ready!(body.poll_data(cx));

        // Log errors.
        if let Some(Err(e)) = &data {
            debug!("http body error: {}", e);
        }

        Poll::Ready(data)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        // Poll the trailers from the body.
        let this = self.project();
        let body = this.body;
        let trailers = ready!(body.poll_trailers(cx));

        // Log errors.
        if let Err(e) = &trailers {
            debug!("http trailers error: {}", e);
        }

        Poll::Ready(trailers)
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

impl<B: Default> Default for UpgradeBody<B> {
    fn default() -> Self {
        Self {
            body: B::default(),
            upgrade: None,
        }
    }
}

impl<B> UpgradeBody<B> {
    pub(crate) fn new(
        body: B,
        upgrade: Option<(Http11Upgrade, hyper::upgrade::OnUpgrade)>,
    ) -> Self {
        Self { body, upgrade }
    }
}

#[pinned_drop]
impl<B> PinnedDrop for UpgradeBody<B> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        // If an HTTP/1 upgrade was wanted, send the upgrade future.
        if let Some((upgrade, on_upgrade)) = this.upgrade.take() {
            if let Err(error) = upgrade.insert_half(on_upgrade) {
                tracing::warn!(
                    ?error,
                    "upgrade body could not send upgrade future upon completion"
                );
            }
        }
    }
}

// === impl HyperConnect ===

impl<C, T> HyperConnect<C, T> {
    pub fn new(connect: C, target: T, absolute_form: bool) -> Self {
        HyperConnect {
            connect,
            target,
            absolute_form,
        }
    }
}

impl<C, T> Service<hyper::Uri> for HyperConnect<C, T>
where
    C: MakeConnection<(linkerd_http_version::Version, T)> + Clone + Send + Sync,
    C::Connection: Unpin + Send,
    C::Future: Unpin + Send + 'static,
    T: Clone + Send + Sync,
{
    type Response = Connection<C::Connection>;
    type Error = Error;
    type Future = HyperConnectFuture<C::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connect.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, _dst: hyper::Uri) -> Self::Future {
        HyperConnectFuture {
            inner: self
                .connect
                .connect((linkerd_http_version::Version::Http1, self.target.clone())),
            absolute_form: self.absolute_form,
        }
    }
}

// === impl HyperConnectFuture ===

impl<F, I, M> Future for HyperConnectFuture<F>
where
    F: TryFuture<Ok = (I, M)> + 'static,
    F::Error: Into<Error>,
{
    type Output = Result<Connection<I>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let (transport, _) = futures::ready!(this.inner.try_poll(cx)).map_err(Into::into)?;
        Poll::Ready(Ok(Connection {
            transport,
            absolute_form: *this.absolute_form,
        }))
    }
}

// === impl Connection ===

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

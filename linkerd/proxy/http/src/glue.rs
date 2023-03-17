use crate::{upgrade::Http11Upgrade, HasH2Reason};
use bytes::Bytes;
use futures::TryFuture;
use hyper::body::HttpBody;
use hyper::client::connect as hyper_connect;
use linkerd_error::{Error, Result};
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
pub struct UpgradeBody {
    /// In UpgradeBody::drop, if this was an HTTP upgrade, the body is taken
    /// to be inserted into the Http11Upgrade half.
    body: hyper::Body,
    pub(super) upgrade: Option<(Http11Upgrade, hyper::upgrade::OnUpgrade)>,
}

/// Glue for a `tower::Service` to used as a `hyper::server::Service`.
#[derive(Debug)]
pub struct HyperServerSvc<S> {
    service: S,
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

impl HttpBody for UpgradeBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let body = self.project().body;
        let poll = futures::ready!(Pin::new(body) // `hyper::Body` is Unpin
            .poll_data(cx));
        Poll::Ready(poll.map(|x| {
            x.map_err(|e| {
                debug!("http body error: {}", e);
                e
            })
        }))
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let body = self.project().body;
        Pin::new(body) // `hyper::Body` is Unpin
            .poll_trailers(cx)
            .map_err(|e| {
                debug!("http trailers error: {}", e);
                e
            })
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

impl Default for UpgradeBody {
    fn default() -> Self {
        hyper::Body::empty().into()
    }
}

impl From<hyper::Body> for UpgradeBody {
    fn from(body: hyper::Body) -> Self {
        Self {
            body,
            upgrade: None,
        }
    }
}

impl UpgradeBody {
    pub(crate) fn new(
        body: hyper::Body,
        upgrade: Option<(Http11Upgrade, hyper::upgrade::OnUpgrade)>,
    ) -> Self {
        Self { body, upgrade }
    }
}

#[pinned_drop]
impl PinnedDrop for UpgradeBody {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        // If an HTTP/1 upgrade was wanted, send the upgrade future.
        if let Some((upgrade, on_upgrade)) = this.upgrade.take() {
            upgrade.insert_half(on_upgrade);
        }
    }
}

// === impl HyperServerSvc ===

impl<S> HyperServerSvc<S> {
    pub fn new(service: S) -> Self {
        HyperServerSvc { service }
    }
}

impl<S> tower::Service<http::Request<hyper::Body>> for HyperServerSvc<S>
where
    S: tower::Service<http::Request<UpgradeBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<hyper::Body>) -> Self::Future {
        self.service.call(req.map(UpgradeBody::from))
    }
}

// === impl HyperConnect ===

impl<C, T> HyperConnect<C, T> {
    pub(super) fn new(connect: C, target: T, absolute_form: bool) -> Self {
        HyperConnect {
            connect,
            target,
            absolute_form,
        }
    }
}

impl<C, T> Service<hyper::Uri> for HyperConnect<C, T>
where
    C: MakeConnection<(crate::Version, T)> + Clone + Send + Sync,
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
                .connect((crate::Version::Http1, self.target.clone())),
            absolute_form: self.absolute_form,
        }
    }
}

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

// === impl Error ===

impl HasH2Reason for hyper::Error {
    fn h2_reason(&self) -> Option<h2::Reason> {
        (self as &(dyn std::error::Error + 'static)).h2_reason()
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

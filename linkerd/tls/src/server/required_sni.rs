use crate::{
    server::{detect_sni, DetectIo},
    ServerName,
};
use linkerd_error::Error;
use linkerd_io as io;
use linkerd_stack::{layer, NewService, Service, ServiceExt};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::time;
use tracing::debug;

#[derive(Clone, Debug, Error)]
#[error("SNI detection timed out")]
pub struct SniDetectionTimeoutError;

#[derive(Clone, Debug, Error)]
#[error("Could not find SNI")]
pub struct NoSniFoundError;

/// A NewService that instruments an inner stack with knowledge of the
/// connection's TLS ServerName (i.e. from an SNI header).
///
/// This differs from the parent module's NewDetectTls in a a few ways:
///
/// - It requires that all connections have an SNI.
/// - It assumes that these connections may not be terminated locally, so there
///   is no concept of a local server name.
/// - There are no special affordances for mutually authenticated TLS, so we
///   make no attempt to detect the client's identity.
/// - The detection timeout is fixed and cannot vary per target (for
///   convenience, to reduce needless boilerplate).
#[derive(Clone, Debug)]
pub struct NewDetectRequiredSni<N> {
    inner: N,
    timeout: time::Duration,
}

#[derive(Clone, Debug)]
pub struct DetectRequiredSni<T, N> {
    target: T,
    inner: N,
    timeout: time::Duration,
}

// === impl NewDetectRequiredSni ===

impl<N> NewDetectRequiredSni<N> {
    fn new(timeout: time::Duration, inner: N) -> Self {
        Self { inner, timeout }
    }

    pub fn layer(timeout: time::Duration) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(timeout, inner))
    }
}

impl<T, N> NewService<T> for NewDetectRequiredSni<N>
where
    N: Clone,
{
    type Service = DetectRequiredSni<T, N>;

    fn new_service(&self, target: T) -> Self::Service {
        DetectRequiredSni::new(self.timeout, target, self.inner.clone())
    }
}

// === impl DetectRequiredSni ===

impl<T, N> DetectRequiredSni<T, N> {
    fn new(timeout: time::Duration, target: T, inner: N) -> Self {
        Self {
            target,
            inner,
            timeout,
        }
    }
}

impl<T, I, N, S> Service<I> for DetectRequiredSni<T, N>
where
    T: Clone + Send + Sync + 'static,
    I: io::AsyncRead + io::Peek + io::AsyncWrite + Send + Sync + Unpin + 'static,
    N: NewService<(ServerName, T), Service = S> + Clone + Send + 'static,
    S: Service<DetectIo<I>> + Send,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let target = self.target.clone();
        let new_accept = self.inner.clone();

        // Detect the SNI from a ClientHello (or timeout).
        let detect = time::timeout(self.timeout, detect_sni(io));
        Box::pin(async move {
            let (res, io) = detect.await.map_err(|_| SniDetectionTimeoutError)??;
            let sni = res.ok_or(NoSniFoundError)?;
            debug!(?sni, "Detected TLS");

            let svc = new_accept.new_service((sni, target));
            svc.oneshot(io).await.map_err(Into::into)
        })
    }
}

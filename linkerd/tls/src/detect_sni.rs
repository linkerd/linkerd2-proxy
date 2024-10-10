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

#[derive(Clone, Debug)]
pub struct NewDetectSni<N> {
    inner: N,
    timeout: time::Duration,
}

#[derive(Clone, Debug)]
pub struct DetectSni<T, N> {
    target: T,
    inner: N,
    timeout: time::Duration,
}

impl<N> NewDetectSni<N> {
    fn new(timeout: time::Duration, inner: N) -> Self {
        Self { inner, timeout }
    }

    pub fn layer(timeout: time::Duration) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(timeout, inner))
    }
}

impl<T, N> NewService<T> for NewDetectSni<N>
where
    N: Clone,
{
    type Service = DetectSni<T, N>;

    fn new_service(&self, target: T) -> Self::Service {
        DetectSni::new(self.timeout, target, self.inner.clone())
    }
}

// === impl DetectSni ===

impl<T, N> DetectSni<T, N> {
    fn new(timeout: time::Duration, target: T, inner: N) -> Self {
        Self {
            target,
            inner,
            timeout,
        }
    }
}

impl<T, I, N, S> Service<I> for DetectSni<T, N>
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
            let (sni, io) = detect.await.map_err(|_| SniDetectionTimeoutError)??;
            let sni = sni.ok_or(NoSniFoundError)?;

            debug!(?sni, "Detected TLS");
            let svc = new_accept.new_service((sni, target));
            svc.oneshot(io).await.map_err(Into::into)
        })
    }
}

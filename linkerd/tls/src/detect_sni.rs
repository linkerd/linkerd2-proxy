use crate::{
    server::{detect_sni, DetectIo, Timeout},
    ServerName,
};
use linkerd_error::Error;
use linkerd_io as io;
use linkerd_stack::{layer, ExtractParam, InsertParam, NewService, Service, ServiceExt};
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
pub struct NewDetectSni<P, N> {
    params: P,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct DetectSni<T, P, N> {
    target: T,
    inner: N,
    timeout: Timeout,
    params: P,
}

impl<P, N> NewDetectSni<P, N> {
    pub fn new(params: P, inner: N) -> Self {
        Self { inner, params }
    }

    pub fn layer(params: P) -> impl layer::Layer<N, Service = Self> + Clone
    where
        P: Clone,
    {
        layer::mk(move |inner| Self::new(params.clone(), inner))
    }
}

impl<T, P, N> NewService<T> for NewDetectSni<P, N>
where
    P: ExtractParam<Timeout, T> + Clone,
    N: Clone,
{
    type Service = DetectSni<T, P, N>;

    fn new_service(&self, target: T) -> Self::Service {
        let timeout = self.params.extract_param(&target);
        DetectSni {
            target,
            timeout,
            inner: self.inner.clone(),
            params: self.params.clone(),
        }
    }
}

impl<T, P, I, N, S> Service<I> for DetectSni<T, P, N>
where
    T: Clone + Send + Sync + 'static,
    P: InsertParam<ServerName, T> + Clone + Send + Sync + 'static,
    P::Target: Send + 'static,
    I: io::AsyncRead + io::Peek + io::AsyncWrite + Send + Sync + Unpin + 'static,
    N: NewService<P::Target, Service = S> + Clone + Send + 'static,
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
        let params = self.params.clone();

        // Detect the SNI from a ClientHello (or timeout).
        let Timeout(timeout) = self.timeout;
        let detect = time::timeout(timeout, detect_sni(io));
        Box::pin(async move {
            let (sni, io) = detect.await.map_err(|_| SniDetectionTimeoutError)??;
            let sni = sni.ok_or(NoSniFoundError)?;

            debug!("detected SNI: {:?}", sni);
            let svc = new_accept.new_service(params.insert_param(sni, target));
            svc.oneshot(io).await.map_err(Into::into)
        })
    }
}

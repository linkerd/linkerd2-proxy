use linkerd_app_core::{
    io,
    svc::{self, layer, ServiceExt},
    tls::{
        server::{detect_sni, CouldNotDetectSNIError, DetectIo, ServerTlsTimeoutError},
        ServerName,
    },
    Error, Result,
};

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

#[derive(Clone, Debug, Default)]
pub struct NewDetectSniServer<N> {
    inner: N,
    timeout: time::Duration,
}

#[derive(Clone, Debug, Default)]
pub struct DetectSniServer<T, N> {
    target: T,
    inner: N,
    timeout: time::Duration,
}

impl<N> NewDetectSniServer<N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Copy {
        let timeout = time::Duration::from_secs(10);
        layer::mk(move |inner| Self { inner, timeout })
    }
}

impl<T, N: Clone> svc::NewService<T> for NewDetectSniServer<N> {
    type Service = DetectSniServer<T, N>;

    fn new_service(&self, target: T) -> Self::Service {
        DetectSniServer {
            target,
            timeout: self.timeout,
            inner: self.inner.clone(),
        }
    }
}

impl<T, I, N, S> svc::Service<I> for DetectSniServer<T, N>
where
    T: Clone + Send + Sync + 'static,
    I: io::AsyncRead + io::Peek + io::AsyncWrite + Send + Sync + Unpin + 'static,
    N: svc::NewService<(ServerName, T), Service = S> + Clone + Send + 'static,
    S: svc::Service<DetectIo<I>> + Send,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let target = self.target.clone();
        let new_accept = self.inner.clone();

        // Detect the SNI from a ClientHello (or timeout).
        let timeout = self.timeout;
        let detect = time::timeout(timeout, detect_sni(io));
        Box::pin(async move {
            let (sni, io) = detect.await.map_err(|_| ServerTlsTimeoutError)??;
            let sni = sni.ok_or(CouldNotDetectSNIError)?;

            println!("detected SNI: {:?}", sni);
            let svc = new_accept.new_service((sni, target));
            svc.oneshot(io).await.map_err(Into::into)
        })
    }
}

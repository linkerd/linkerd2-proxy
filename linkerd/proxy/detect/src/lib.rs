use async_trait::async_trait;
use futures::prelude::*;
use linkerd2_error::Error;
use linkerd2_io::{AsyncRead, AsyncWrite, BoxedIo};
use linkerd2_proxy_core as core;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::util::ServiceExt;

/// A strategy for detecting values out of a client transport.
#[async_trait]
pub trait Detect<T, I: AsyncRead + AsyncWrite> {
    type Target;
    type Io: AsyncRead + AsyncWrite + Send + Unpin;
    type Error: Into<Error>;

    async fn detect(&self, target: T, io: I) -> Result<(Self::Target, Self::Io), Self::Error>;
}

#[derive(Debug, Clone)]
pub struct DetectProtocolLayer<D> {
    detect: D,
}

#[derive(Debug, Clone)]
pub struct DetectProtocol<D, A> {
    detect: D,
    accept: A,
}

impl<D> DetectProtocolLayer<D> {
    pub fn new(detect: D) -> Self {
        Self { detect }
    }
}

impl<D: Clone, A> tower::layer::Layer<A> for DetectProtocolLayer<D> {
    type Service = DetectProtocol<D, A>;

    fn layer(&self, accept: A) -> Self::Service {
        Self::Service::new(self.detect.clone(), accept)
    }
}

impl<D: Clone, A> DetectProtocol<D, A> {
    pub fn new(detect: D, accept: A) -> Self {
        Self { detect, accept }
    }
}

impl<T, I, D, A> tower::Service<(T, I)> for DetectProtocol<D, A>
where
    T: Send + 'static,
    I: AsyncRead + AsyncWrite + Send + 'static,
    D: Detect<T, I, Io = BoxedIo> + Clone + Send + 'static,
    D::Target: Send,
    A: core::Accept<(D::Target, BoxedIo)> + Send + Clone + 'static,
    A::Future: Send,
{
    type Response = A::ConnectionFuture;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<A::ConnectionFuture, Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The `accept` is cloned into the response future, so its readiness isn't important.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (target, io): (T, I)) -> Self::Future {
        let detect = self.detect.clone();
        let mut accept = self.accept.clone().into_service();
        Box::pin(async move {
            // Await the service and protocol detection together. If either fails, the other is
            // aborted.
            let (accept, conn) = futures::try_join!(
                accept.ready_and().map_err(Into::into),
                detect.detect(target, io).map_err(Into::into)
            )?;

            accept.call(conn).await.map_err(Into::into)
        })
    }
}

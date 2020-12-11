use crate::io;
use linkerd2_error::Error;
use linkerd2_stack::{layer, NewService};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::util::ServiceExt;

#[async_trait::async_trait]
pub trait Detect<I>: Clone + Send + Sync + 'static {
    type Kind: Send;

    async fn detect(&self, io: I) -> Result<(Self::Kind, io::PrefixedIo<I>), Error>;
}

#[derive(Copy, Clone)]
pub struct NewDetectService<N, D> {
    new_accept: N,
    detect: D,
}

#[derive(Copy, Clone)]
pub struct DetectService<N, D, T> {
    target: T,
    new_accept: N,
    detect: D,
}

impl<N, D: Clone> NewDetectService<N, D> {
    pub fn new(new_accept: N, detect: D) -> Self {
        Self { new_accept, detect }
    }

    pub fn layer(detect: D) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new| Self::new(new, detect.clone()))
    }
}

impl<N: Clone, D: Clone, T> NewService<T> for NewDetectService<N, D> {
    type Service = DetectService<N, D, T>;

    fn new_service(&mut self, target: T) -> DetectService<N, D, T> {
        DetectService {
            target,
            new_accept: self.new_accept.clone(),
            detect: self.detect.clone(),
        }
    }
}

impl<N, S, D, T, I> tower::Service<I> for DetectService<N, D, T>
where
    T: Clone + Send + 'static,
    I: Send + 'static,
    D: Detect<I>,
    N: NewService<(D::Kind, T), Service = S> + Clone + Send + 'static,
    S: tower::Service<io::PrefixedIo<I>, Response = ()> + Send,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(().into()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let mut new_accept = self.new_accept.clone();
        let detect = self.detect.clone();
        let target = self.target.clone();
        Box::pin(async move {
            let (kind, io) = detect.detect(io).await?;
            let mut accept = new_accept
                .new_service((kind, target))
                .ready_oneshot()
                .await
                .map_err(Into::into)?;
            accept.call(io).await.map_err(Into::into)?;
            // Hold the service until it's done being used.
            drop(accept);
            Ok(())
        })
    }
}

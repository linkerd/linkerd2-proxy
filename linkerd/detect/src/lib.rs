#![deny(warnings, rust_2018_idioms)]

mod timeout;

pub use self::timeout::{DetectTimeout, DetectTimeoutError};
use bytes::BytesMut;
use futures::prelude::*;
use linkerd_error::Error;
use linkerd_io as io;
use linkerd_stack::{layer, NewService};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;
use tower::util::ServiceExt;
use tracing::{debug, trace};

#[async_trait::async_trait]
pub trait Detect: Clone + Send + Sync + 'static {
    type Protocol: Send;

    async fn detect<I: io::AsyncRead + Send + Unpin + 'static>(
        &self,
        io: &mut I,
        buf: &mut BytesMut,
    ) -> Result<Option<Self::Protocol>, Error>;
}

#[derive(Copy, Clone, Debug)]
pub struct NewDetectService<D, N> {
    inner: N,
    detect: D,
    capacity: usize,
}

#[derive(Copy, Clone, Debug)]
pub struct DetectService<T, D, N> {
    target: T,
    inner: N,
    detect: D,
    capacity: usize,
}

const BUFFER_CAPACITY: usize = 1024;

// === impl NewDetectService ===

impl<D: Clone, N> NewDetectService<D, N> {
    pub fn new(detect: D, inner: N) -> Self {
        Self {
            detect,
            inner,
            capacity: BUFFER_CAPACITY,
        }
    }

    pub fn layer(detect: D) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(detect.clone(), inner))
    }
}

impl<D: Clone, N> NewDetectService<DetectTimeout<D>, N> {
    pub fn timeout(
        timeout: time::Duration,
        detect: D,
    ) -> impl layer::Layer<N, Service = NewDetectService<DetectTimeout<D>, N>> + Clone {
        Self::layer(DetectTimeout::new(timeout, detect))
    }
}

impl<D: Clone, N: Clone, T> NewService<T> for NewDetectService<D, N> {
    type Service = DetectService<T, D, N>;

    fn new_service(&mut self, target: T) -> DetectService<T, D, N> {
        DetectService::new(target, self.detect.clone(), self.inner.clone())
    }
}

// === impl DetectService ===

impl<T, D: Clone, N: Clone> DetectService<T, D, N> {
    pub fn new(target: T, detect: D, inner: N) -> Self {
        DetectService {
            target,
            detect,
            inner,
            capacity: BUFFER_CAPACITY,
        }
    }
}

impl<S, T, D, N, I> tower::Service<I> for DetectService<T, D, N>
where
    T: Clone + Send + 'static,
    I: io::AsyncRead + Send + Unpin + 'static,
    D: Detect,
    D::Protocol: std::fmt::Debug,
    N: NewService<(Option<D::Protocol>, T), Service = S> + Clone + Send + 'static,
    S: tower::Service<io::PrefixedIo<I>, Response = ()> + Send,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut io: I) -> Self::Future {
        let mut inner = self.inner.clone();
        let mut buf = BytesMut::with_capacity(self.capacity);
        let detect = self.detect.clone();
        let target = self.target.clone();
        Box::pin(async move {
            trace!("Starting protocol detection");
            let t0 = time::Instant::now();
            let protocol = detect.detect(&mut io, &mut buf).await?;
            debug!(
                ?protocol,
                elapsed = ?(time::Instant::now() - t0),
                "Detected"
            );

            let mut accept = inner
                .new_service((protocol, target))
                .ready_oneshot()
                .err_into::<Error>()
                .await?;

            trace!("Dispatching connection");
            accept
                .call(io::PrefixedIo::new(buf.freeze(), io))
                .err_into::<Error>()
                .await?;

            trace!("Connection completed");
            // Hold the service until it's done being used so that cache
            // idleness is reset.
            drop(accept);

            Ok(())
        })
    }
}

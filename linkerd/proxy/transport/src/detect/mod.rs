mod timeout;

pub use self::timeout::{DetectTimeout, DetectTimeoutError};
use crate::io;
use bytes::BytesMut;
use futures::prelude::*;
use linkerd2_error::Error;
use linkerd2_stack::{layer, NewService};
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

#[derive(Copy, Clone)]
pub struct NewDetectService<N, D> {
    new_accept: N,
    detect: D,
    capacity: usize,
}

#[derive(Copy, Clone)]
pub struct DetectService<N, D, T> {
    target: T,
    new_accept: N,
    detect: D,
    capacity: usize,
}

// === impl NewDetectService ===

impl<N, D: Clone> NewDetectService<N, D> {
    const BUFFER_CAPACITY: usize = 1024;

    pub fn new(new_accept: N, detect: D) -> Self {
        Self {
            detect,
            new_accept,
            capacity: Self::BUFFER_CAPACITY,
        }
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
            capacity: self.capacity,
        }
    }
}

// === impl DetectService ===

impl<N, S, D, T, I> tower::Service<I> for DetectService<N, D, T>
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
        Poll::Ready(Ok(().into()))
    }

    fn call(&mut self, mut io: I) -> Self::Future {
        let mut new_accept = self.new_accept.clone();
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

            let mut accept = new_accept
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

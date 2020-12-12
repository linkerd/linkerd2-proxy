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
use tracing::trace;

#[async_trait::async_trait]
pub trait Detect: Clone + Send + Sync + 'static {
    type Kind: Send;

    async fn detect<I: io::AsyncRead + Send + Unpin + 'static>(
        &self,
        io: &mut I,
        buf: &mut BytesMut,
    ) -> Result<Self::Kind, Error>;
}

#[derive(Copy, Clone)]
pub struct NewDetectService<N, D> {
    new_accept: N,
    detect: D,
    capacity: usize,
    timeout: time::Duration,
}

#[derive(Copy, Clone)]
pub struct DetectService<N, D, T> {
    target: T,
    new_accept: N,
    detect: D,
    capacity: usize,
    timeout: time::Duration,
}

#[derive(Debug)]
pub struct DetectTimeout {
    bytes: usize,
    timeout: time::Duration,
}

// === impl NewDetectService ===

impl<N, D: Clone> NewDetectService<N, D> {
    const BUFFER_CAPACITY: usize = 8192;

    pub fn new(timeout: time::Duration, new_accept: N, detect: D) -> Self {
        Self {
            detect,
            new_accept,
            capacity: Self::BUFFER_CAPACITY,
            timeout,
        }
    }

    pub fn layer(
        timeout: time::Duration,
        detect: D,
    ) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new| Self::new(timeout, new, detect.clone()))
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
            timeout: self.timeout,
        }
    }
}

// === impl DetectService ===

impl<N, S, D, T, I> tower::Service<I> for DetectService<N, D, T>
where
    T: Clone + Send + 'static,
    I: io::AsyncRead + Send + Unpin + 'static,
    D: Detect,
    D::Kind: std::fmt::Debug,
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

    fn call(&mut self, mut io: I) -> Self::Future {
        let mut new_accept = self.new_accept.clone();
        let mut buf = BytesMut::with_capacity(self.capacity);
        let detect = self.detect.clone();
        let target = self.target.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            trace!("Starting protocol detection");
            let kind = futures::select_biased! {
                res = detect.detect(&mut io, &mut buf).fuse() => res?,
                _ = time::sleep(timeout).fuse() => {
                    let bytes = buf.len();
                    return Err(DetectTimeout { bytes, timeout }.into());
                }
            };

            trace!(?kind, "Creating service");
            let mut accept = new_accept
                .new_service((kind, target))
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

// === impl DetectTimeout ===

impl std::fmt::Display for DetectTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Protocol detection timeout after: {}B after {:?}",
            self.bytes, self.timeout
        )
    }
}

impl std::error::Error for DetectTimeout {}

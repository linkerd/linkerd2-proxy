#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

use bytes::BytesMut;
use linkerd_error::Error;
use linkerd_io as io;
use linkerd_stack::{layer, NewService};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::time;
use tower::util::ServiceExt;
use tracing::{debug, info, trace};

#[async_trait::async_trait]
pub trait Detect<I>: Clone + Send + Sync + 'static {
    type Protocol: Send;

    async fn detect(&self, io: &mut I, buf: &mut BytesMut)
        -> Result<Option<Self::Protocol>, Error>;
}

pub type DetectResult<P> = Result<Option<P>, DetectTimeoutError<P>>;

#[derive(Error)]
#[error("{} protocol detection timed out after {:?}", std::any::type_name::<P>(), .0)]
pub struct DetectTimeoutError<P>(time::Duration, std::marker::PhantomData<P>);

#[derive(Copy, Clone, Debug)]
pub struct NewDetectService<D, N> {
    inner: N,
    detect: D,
    capacity: usize,
    timeout: time::Duration,
}

#[derive(Copy, Clone, Debug)]
pub struct DetectService<T, D, N> {
    target: T,
    inner: N,
    detect: D,
    capacity: usize,
    timeout: time::Duration,
}

const BUFFER_CAPACITY: usize = 1024;

pub fn allow_timeout<P, T>((p, t): (DetectResult<P>, T)) -> (Option<P>, T) {
    match p {
        Ok(p) => (p, t),
        Err(e) => {
            info!("Continuing after timeout: {}", e);
            (None, t)
        }
    }
}

// === impl NewDetectService ===

impl<D: Clone, N> NewDetectService<D, N> {
    pub fn new(timeout: time::Duration, detect: D, inner: N) -> Self {
        Self {
            detect,
            inner,
            timeout,
            capacity: BUFFER_CAPACITY,
        }
    }

    pub fn layer(
        timeout: time::Duration,
        detect: D,
    ) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(timeout, detect.clone(), inner))
    }
}

impl<D: Clone, N: Clone, T> NewService<T> for NewDetectService<D, N> {
    type Service = DetectService<T, D, N>;

    fn new_service(&mut self, target: T) -> DetectService<T, D, N> {
        DetectService {
            target,
            detect: self.detect.clone(),
            inner: self.inner.clone(),
            capacity: self.capacity,
            timeout: self.timeout,
        }
    }
}

// === impl DetectService ===

impl<S, T, D, N, I> tower::Service<I> for DetectService<T, D, N>
where
    T: Clone + Send + 'static,
    I: Send + 'static,
    D: Detect<I>,
    D::Protocol: std::fmt::Debug,
    N: NewService<(DetectResult<D::Protocol>, T), Service = S> + Clone + Send + 'static,
    S: tower::Service<io::PrefixedIo<I>, Response = ()> + Send,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut io: I) -> Self::Future {
        let mut inner = self.inner.clone();
        let mut buf = BytesMut::with_capacity(self.capacity);
        let detect = self.detect.clone();
        let target = self.target.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            trace!("Starting protocol detection");
            let t0 = time::Instant::now();

            let detected = match time::timeout(timeout, detect.detect(&mut io, &mut buf)).await {
                Ok(Ok(protocol)) => {
                    debug!(?protocol, elapsed = ?t0.elapsed(), "DetectResult");
                    Ok(protocol)
                }
                Err(_) => Err(DetectTimeoutError(timeout, std::marker::PhantomData)),
                Ok(Err(e)) => return Err(e),
            };

            let mut accept = inner
                .new_service((detected, target))
                .ready_oneshot()
                .await
                .map_err(Into::into)?;

            trace!("Dispatching connection");
            accept
                .call(io::PrefixedIo::new(buf.freeze(), io))
                .await
                .map_err(Into::into)?;

            trace!("Connection completed");
            // Hold the service until it's done being used so that cache
            // idleness is reset.
            drop(accept);

            Ok(())
        })
    }
}

// === impl DetectTimeoutError ===

impl<P> fmt::Debug for DetectTimeoutError<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple(std::any::type_name::<Self>())
            .field(&self.0)
            .finish()
    }
}

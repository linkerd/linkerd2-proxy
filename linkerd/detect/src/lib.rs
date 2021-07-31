#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

use bytes::BytesMut;
use linkerd_error::Error;
use linkerd_io as io;
use linkerd_stack::{layer, ExtractParam, NewService};
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
#[error("{} protocol detection timed out after {0:?}", std::any::type_name::<P>())]
pub struct DetectTimeoutError<P>(time::Duration, std::marker::PhantomData<P>);

#[derive(Copy, Clone, Debug)]
pub struct Config<D> {
    pub detect: D,
    pub capacity: usize,
    pub timeout: time::Duration,
}

#[derive(Copy, Clone, Debug)]
pub struct NewDetectService<P, D, N> {
    inner: N,
    params: P,
    _detect: std::marker::PhantomData<fn() -> D>,
}

#[derive(Copy, Clone, Debug)]
pub struct DetectService<T, D, N> {
    target: T,
    config: Config<D>,
    inner: N,
}

pub fn allow_timeout<P, T>((p, t): (DetectResult<P>, T)) -> (Option<P>, T) {
    match p {
        Ok(p) => (p, t),
        Err(e) => {
            info!("Continuing after timeout: {}", e);
            (None, t)
        }
    }
}

// === impl Config ===

impl<D: Default> Config<D> {
    const DEFAULT_CAPACITY: usize = 1024;

    pub fn from_timeout(timeout: time::Duration) -> Self {
        Self {
            detect: D::default(),
            capacity: Self::DEFAULT_CAPACITY,
            timeout,
        }
    }
}

// === impl NewDetectService ===

impl<P, D, N> NewDetectService<P, D, N> {
    pub fn new(params: P, inner: N) -> Self {
        Self {
            inner,
            params,
            _detect: std::marker::PhantomData,
        }
    }

    pub fn layer(params: P) -> impl layer::Layer<N, Service = Self> + Clone
    where
        P: Clone,
    {
        layer::mk(move |inner| Self::new(params.clone(), inner))
    }
}

impl<T, P, D, N: Clone> NewService<T> for NewDetectService<P, D, N>
where
    P: ExtractParam<Config<D>, T>,
{
    type Service = DetectService<T, D, N>;

    fn new_service(&mut self, target: T) -> DetectService<T, D, N> {
        let config = self.params.extract_param(&target);
        DetectService {
            target,
            config,
            inner: self.inner.clone(),
        }
    }
}

// === impl DetectService ===

impl<I, T, D, N, NSvc> tower::Service<I> for DetectService<T, D, N>
where
    T: Clone + Send + 'static,
    I: Send + 'static,
    D: Detect<I>,
    D::Protocol: std::fmt::Debug,
    N: NewService<(DetectResult<D::Protocol>, T), Service = NSvc> + Clone + Send + 'static,
    NSvc: tower::Service<io::PrefixedIo<I>, Response = ()> + Send,
    NSvc::Error: Into<Error>,
    NSvc::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut io: I) -> Self::Future {
        let Config {
            detect,
            capacity,
            timeout,
        } = self.config.clone();
        let target = self.target.clone();
        let mut inner = self.inner.clone();
        Box::pin(async move {
            trace!("Starting protocol detection");
            let t0 = time::Instant::now();

            let mut buf = BytesMut::with_capacity(capacity);
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

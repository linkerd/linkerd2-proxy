#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use bytes::BytesMut;
use linkerd_error::Error;
use linkerd_io as io;
use linkerd_stack::{layer, ExtractParam, NewService};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    result::Result as StdResult,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::time;
use tower::util::ServiceExt;
use tracing::{debug, info, trace};

#[async_trait::async_trait]
pub trait Detect<I>: Clone + Send + Sync + 'static {
    type Protocol: Send;

    async fn detect(
        &self,
        io: &mut I,
        buf: &mut BytesMut,
    ) -> StdResult<Option<Self::Protocol>, Error>;
}

pub type Result<P> = StdResult<Option<P>, DetectTimeoutError<P>>;

#[derive(Error)]
#[error("{typ} protocol detection timed out after {0:?}", typ=std::any::type_name::<P>())]
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
pub struct DetectService<D, N> {
    config: Config<D>,
    inner: N,
}

pub fn allow_timeout<P>(p: Result<P>) -> Option<P> {
    match p {
        Ok(p) => p,
        Err(e) => {
            info!("Continuing after timeout: {}", e);
            None
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

impl<T, P, D, N> NewService<T> for NewDetectService<P, D, N>
where
    P: ExtractParam<Config<D>, T>,
    N: NewService<T>,
{
    type Service = DetectService<D, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let config = self.params.extract_param(&target);
        DetectService {
            config,
            inner: self.inner.new_service(target),
        }
    }
}

// === impl DetectService ===

impl<I, D, N, NSvc> tower::Service<I> for DetectService<D, N>
where
    I: Send + 'static,
    D: Detect<I>,
    D::Protocol: std::fmt::Debug,
    N: NewService<Result<D::Protocol>, Service = NSvc> + Clone + Send + 'static,
    NSvc: tower::Service<io::PrefixedIo<I>, Response = ()> + Send,
    NSvc::Error: Into<Error>,
    NSvc::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = StdResult<(), Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<StdResult<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut io: I) -> Self::Future {
        let Config {
            detect,
            capacity,
            timeout,
        } = self.config.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            trace!(%capacity, ?timeout, "Starting protocol detection");
            let t0 = time::Instant::now();

            let mut buf = BytesMut::with_capacity(capacity);
            let detected = match time::timeout(timeout, detect.detect(&mut io, &mut buf)).await {
                Ok(Ok(protocol)) => {
                    debug!(
                        ?protocol,
                        elapsed = ?time::Instant::now().saturating_duration_since(t0),
                        "Detected protocol",
                    );
                    Ok(protocol)
                }
                Err(_) => Err(DetectTimeoutError(timeout, std::marker::PhantomData)),
                Ok(Err(e)) => return Err(e),
            };

            trace!("Dispatching connection");
            let svc = inner.new_service(detected);
            let mut svc = svc.ready_oneshot().await.map_err(Into::into)?;
            svc.call(io::PrefixedIo::new(buf.freeze(), io))
                .await
                .map_err(Into::into)?;

            trace!("Connection completed");
            // Hold the service until it's done being used so that cache
            // idleness is reset.
            drop(svc);

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

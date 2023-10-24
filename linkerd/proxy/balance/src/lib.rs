#![allow(warnings)]

use futures::prelude::*;
use linkerd_error::Error;
use linkerd_proxy_core::Resolve;
use linkerd_proxy_pool::PoolQ;
use linkerd_stack::{layer, Gate, NewService, Param, Service};
use std::{fmt::Debug, hash::Hash, marker::PhantomData, net::SocketAddr};
use tokio::time;
use tower::load::{self, PeakEwma};

mod discover;
mod gauge_endpoints;
mod p2c_pool;

use crate::p2c_pool::P2cPool;

pub use self::{
    discover::DiscoveryStreamOverflow,
    gauge_endpoints::{EndpointsGauges, NewGaugeEndpoints},
};
pub use tower::load::peak_ewma::Handle;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EwmaConfig {
    pub default_rtt: time::Duration,
    pub decay: time::Duration,
}

/// Configures a stack to resolve targets to balance requests over `N`-typed
/// endpoint stacks.
#[derive(Debug)]
pub struct NewBalancePeakEwma<C, Req, R, N> {
    update_queue_capacity: usize,
    resolve: R,
    inner: N,
    _marker: PhantomData<fn(Req) -> C>,
}

pub type Balance<Req, F> = Gate<PoolQ<Req, F>>;

/// Wraps the inner services in [`PeakEwma`] services so their load is tracked
/// for the p2c balancer.
#[derive(Debug)]
pub struct NewPeakEwma<C, Req, N> {
    config: EwmaConfig,
    inner: N,
    _marker: PhantomData<fn(Req) -> C>,
}

// === impl NewBalancePeakEwma ===

impl<C, Req, R, N> NewBalancePeakEwma<C, Req, R, N> {
    /// Limits the number of endpoint updates that can be buffered by a
    /// discovery stream (i.e., for a specific service resolution).
    ///
    /// The buffering task ensures that discovery updates are processed (i.e.,
    /// from the controller client) even when the balancer is not processing new
    /// requests. If the buffer fills up, we'll stop polling the discovery
    /// stream. When the stream represents an gRPC streaming response, the
    /// server may become unable to write further updates when the buffer is
    /// full.
    ///
    /// 1K updates should be more than enough for most load balancers.
    const UPDATE_QUEUE_CAPACITY: usize = 1_000;

    pub fn new(inner: N, resolve: R) -> Self {
        Self {
            update_queue_capacity: Self::UPDATE_QUEUE_CAPACITY,
            resolve,
            inner,
            _marker: PhantomData,
        }
    }

    pub fn layer<T>(resolve: R) -> impl layer::Layer<N, Service = Self> + Clone
    where
        R: Clone,
        Self: NewService<T>,
    {
        layer::mk(move |inner| Self::new(inner, resolve.clone()))
    }
}

impl<C, T, Req, R, M, N, S> NewService<T> for NewBalancePeakEwma<C, Req, R, M>
where
    T: Param<EwmaConfig> + Clone + Send,
    R: Resolve<T>,
    R::Resolution: Unpin,
    R::Error: Send,
    M: NewService<T, Service = N> + Clone,
    N: NewService<(SocketAddr, R::Endpoint), Service = S> + Send + 'static,
    S: Service<Req> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Error>,
    C: load::TrackCompletion<load::peak_ewma::Handle, S::Response> + Default + Send + 'static,
    Req: Send + 'static,
    Balance<Req, future::ErrInto<<PeakEwma<S, C> as Service<Req>>::Future, Error>>: Service<Req>,
{
    type Service = Balance<Req, future::ErrInto<<PeakEwma<S, C> as Service<Req>>::Future, Error>>;

    fn new_service(&self, target: T) -> Self::Service {
        // TODO(ver) QueueConfig.
        const FAILFAST: time::Duration = time::Duration::from_secs(10);
        const CAPACITY: usize = 10_000;

        let disco = self.resolve.resolve(target.clone()).try_flatten_stream();

        let pool = P2cPool::new(NewPeakEwma::new(
            target.param(),
            self.inner.new_service(target),
        ));

        PoolQ::spawn(CAPACITY, FAILFAST, disco, pool)
    }
}

impl<C, Req, R: Clone, N: Clone> Clone for NewBalancePeakEwma<C, Req, R, N> {
    fn clone(&self) -> Self {
        Self {
            update_queue_capacity: self.update_queue_capacity,
            resolve: self.resolve.clone(),
            inner: self.inner.clone(),
            _marker: self._marker,
        }
    }
}

// === impl NewPeakEwma ===

impl<C, Req, N> NewPeakEwma<C, Req, N> {
    fn new(config: EwmaConfig, inner: N) -> Self {
        Self {
            config,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<C, T, N, Req, S> NewService<T> for NewPeakEwma<C, Req, N>
where
    C: load::TrackCompletion<load::peak_ewma::Handle, S::Response> + Default,
    N: NewService<T, Service = S>,
    S: Service<Req>,
{
    type Service = PeakEwma<S, C>;

    fn new_service(&self, target: T) -> Self::Service {
        // Converts durations to nanos in f64.
        //
        // Due to a lossy transformation, the maximum value that can be
        // represented is ~585 years, which, I hope, is more than enough to
        // represent request latencies.
        fn nanos(d: time::Duration) -> f64 {
            const NANOS_PER_SEC: u64 = 1_000_000_000;
            let n = f64::from(d.subsec_nanos());
            let s = d.as_secs().saturating_mul(NANOS_PER_SEC) as f64;
            n + s
        }

        PeakEwma::new(
            self.inner.new_service(target),
            self.config.default_rtt,
            nanos(self.config.decay),
            C::default(),
        )
    }
}

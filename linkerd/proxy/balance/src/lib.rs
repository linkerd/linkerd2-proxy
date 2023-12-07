#![allow(warnings)]

use futures::prelude::*;
use linkerd_error::Error;
use linkerd_metrics::prom;
use linkerd_proxy_core::Resolve;
use linkerd_proxy_pool::PoolQueue;
use linkerd_stack::{layer, queue, ExtractParam, Gate, NewService, Param, Service};
use std::{fmt::Debug, hash::Hash, marker::PhantomData, net::SocketAddr};
use tokio::time;
use tower::load::{self, PeakEwma};

mod discover;
mod gauge_endpoints;
mod pool;

pub use self::{
    discover::DiscoveryStreamOverflow,
    gauge_endpoints::{EndpointsGauges, NewGaugeEndpoints},
    pool::{
        MetricFamilies, Metrics, P2cMetricFamilies, P2cMetrics, P2cPool, Pool, QueueMetricFamilies,
        QueueMetrics, Update,
    },
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
pub struct NewBalancePeakEwma<C, Req, X, R, N> {
    resolve: R,
    inner: N,
    params: X,
    _marker: PhantomData<fn(Req) -> C>,
}

pub type Balance<Req, F> = Gate<PoolQueue<Req, F>>;

/// Wraps the inner services in [`PeakEwma`] services so their load is tracked
/// for the p2c balancer.
#[derive(Debug)]
pub struct NewPeakEwma<C, Req, N> {
    config: EwmaConfig,
    inner: N,
    _marker: PhantomData<fn(Req) -> C>,
}

// === impl NewBalancePeakEwma ===

impl<C, Req, X, R, N> NewBalancePeakEwma<C, Req, X, R, N> {
    pub fn new(inner: N, resolve: R, params: X) -> Self {
        Self {
            resolve,
            inner,
            params,
            _marker: PhantomData,
        }
    }

    pub fn layer<T>(resolve: R, params: X) -> impl layer::Layer<N, Service = Self> + Clone
    where
        R: Clone,
        X: Clone,
        Self: NewService<T>,
    {
        layer::mk(move |inner| Self::new(inner, resolve.clone(), params.clone()))
    }
}

impl<C, T, Req, X, R, M, N, S> NewService<T> for NewBalancePeakEwma<C, Req, X, R, M>
where
    T: Param<EwmaConfig> + Param<queue::Capacity> + Param<queue::Timeout> + Clone + Send,
    X: ExtractParam<pool::Metrics, T>,
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
        // Initialize a resolution stream to discover endpoint updates. This
        // stream should be effectively inifite (and, i.e., handle errors
        // gracefully).
        //
        // If the resolution stream ends, the balancer will simply stop
        // processing endpoint updates.
        //
        // If the resolution stream fails, the balancer will return an error.
        let disco = self.resolve.resolve(target.clone()).try_flatten_stream();

        let queue::Capacity(capacity) = target.param();
        let queue::Timeout(failfast) = target.param();
        let metrics = self.params.extract_param(&target);

        // The pool wraps the inner endpoint stack so that its inner ready cache
        // can be updated without requiring the service to process requests.
        let pool = {
            let ewma = target.param();
            let new_endpoint = self.inner.new_service(target);
            P2cPool::new(NewPeakEwma::new(ewma, new_endpoint))
        };

        // The queue runs on a dedicated task, owning the resolution stream and
        // all of the inner endpoint services. A cloneable Service is returned
        // that allows passing requests to the service. When all clones of the
        // service are dropped, the queue task completes, dropping the
        // resolution and all inner services.
        PoolQueue::spawn(capacity, failfast, metrics.queue, disco, pool)
    }
}

impl<C, Req, X: Clone, R: Clone, N: Clone> Clone for NewBalancePeakEwma<C, Req, X, R, N> {
    fn clone(&self) -> Self {
        Self {
            resolve: self.resolve.clone(),
            inner: self.inner.clone(),
            params: self.params.clone(),
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

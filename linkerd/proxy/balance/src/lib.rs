mod discover;

use linkerd_error::Error;
use linkerd_proxy_core::Resolve;
use linkerd_stack::{layer, NewService, Param, Service};
use rand::thread_rng;
use std::{fmt::Debug, marker::PhantomData, net::SocketAddr, time::Duration};
use tower::{
    balance::p2c,
    load::{self, PeakEwma},
};

pub use tower::load::peak_ewma::Handle;

#[derive(Copy, Clone, Debug, Default)]
pub struct EwmaConfig {
    pub default_rtt: Duration,
    pub decay: Duration,
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

type Buffer<C, S> = discover::Buffer<PeakEwma<S, C>>;
pub type Balance<C, Req, S> = p2c::Balance<Buffer<C, S>, Req>;

/// Wraps the inner stack in [`NewPeakEwma`] to produce [`PeakEwma`] services.
#[derive(Debug)]
pub struct NewNewPeakEwma<C, Req, N> {
    inner: N,
    _marker: PhantomData<fn(Req) -> C>,
}

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
    /// Limits the number of endpoint updates that can be buffered by a discover
    /// stream.
    ///
    /// The buffering task ensures that discovery updates are processed (i.e.,
    /// from the controller client) even when the balancer is not processing new
    /// requests.
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

    pub fn layer(resolve: R) -> impl layer::Layer<N, Service = Self> + Clone
    where
        R: Clone,
    {
        layer::mk(move |inner| Self::new(inner, resolve.clone()))
    }
}

impl<C, T, Req, R, M, N, S> NewService<T> for NewBalancePeakEwma<C, Req, R, M>
where
    T: Param<EwmaConfig> + Clone + Send,
    R: Resolve<T>,
    R::Endpoint: Clone + Debug + Eq + Send + 'static,
    R::Resolution: Send,
    R::Future: Send + 'static,
    M: NewService<T, Service = N> + Clone,
    N: NewService<(SocketAddr, R::Endpoint), Service = S> + Send + 'static,
    S: Service<Req> + Send,
    S::Error: Into<Error>,
    C: load::TrackCompletion<load::peak_ewma::Handle, S::Response> + Default + Send + 'static,
    Req: 'static,
    Balance<C, Req, S>: Service<Req>,
{
    type Service = Balance<C, Req, S>;

    fn new_service(&self, target: T) -> Self::Service {
        let ewma = NewNewPeakEwma {
            inner: self.inner.clone(),
            _marker: PhantomData,
        };
        let disco = discover::spawn_new(
            self.update_queue_capacity,
            self.resolve.clone(),
            ewma,
            target,
        );

        Balance::from_rng(disco, &mut thread_rng()).expect("RNG must be valid")
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

// === impl NewNewPeakEwma ===

impl<C, T, N, Req> NewService<T> for NewNewPeakEwma<C, Req, N>
where
    T: Param<EwmaConfig>,
    N: NewService<T>,
{
    type Service = NewPeakEwma<C, Req, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let config = target.param();
        let inner = self.inner.new_service(target);
        NewPeakEwma {
            config,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<C, Req, N: Clone> Clone for NewNewPeakEwma<C, Req, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: self._marker,
        }
    }
}

// === impl NewPeakEwma ===

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
        fn nanos(d: Duration) -> f64 {
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

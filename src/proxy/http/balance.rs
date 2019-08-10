use crate::svc;
use futures::{try_ready, Async, Future, Poll};
use http;
use hyper::body::Payload;
pub use hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
use rand::{rngs::SmallRng, FromEntropy};
use std::{marker::PhantomData, time::Duration};
pub use tower_balance::p2c::Balance;
use tower_discover::Discover;
pub use tower_load::{Load, PeakEwmaDiscover};

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct Layer<A, B> {
    decay: Duration,
    default_rtt: Duration,
    rng: SmallRng,
    _marker: PhantomData<fn(A) -> B>,
}

/// Resolves `T` typed targets to balance requests over `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct MakeSvc<M, A, B> {
    decay: Duration,
    default_rtt: Duration,
    inner: M,
    rng: SmallRng,
    _marker: PhantomData<fn(A) -> B>,
}

// === impl Layer ===

pub fn layer<A, B>(default_rtt: Duration, decay: Duration) -> Layer<A, B> {
    Layer {
        decay,
        default_rtt,
        rng: SmallRng::from_entropy(),
        _marker: PhantomData,
    }
}

impl<A, B> Clone for Layer<A, B> {
    fn clone(&self) -> Self {
        Self {
            decay: self.decay,
            default_rtt: self.default_rtt,
            rng: self.rng.clone(),
            _marker: PhantomData,
        }
    }
}

impl<M, A, B> svc::Layer<M> for Layer<A, B>
where
    A: Payload,
    B: Payload,
{
    type Service = MakeSvc<M, A, B>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            decay: self.decay,
            default_rtt: self.default_rtt,
            inner,
            rng: self.rng.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl MakeSvc ===

impl<M: Clone, A, B> Clone for MakeSvc<M, A, B> {
    fn clone(&self) -> Self {
        MakeSvc {
            decay: self.decay,
            default_rtt: self.default_rtt,
            inner: self.inner.clone(),
            rng: self.rng.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T, M, A, B> svc::Service<T> for MakeSvc<M, A, B>
where
    M: svc::Service<T>,
    M::Response: Discover,
    <M::Response as Discover>::Service:
        svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Payload,
    B: Payload,
    Balance<PeakEwmaDiscover<M::Response, PendingUntilFirstData>, http::Request<A>>:
        svc::Service<http::Request<A>>,
{
    type Response = Balance<PeakEwmaDiscover<M::Response, PendingUntilFirstData>, http::Request<A>>;
    type Error = M::Error;
    type Future = MakeSvc<M::Future, A, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.call(target);

        MakeSvc {
            decay: self.decay,
            default_rtt: self.default_rtt,
            inner,
            rng: self.rng.clone(),
            _marker: PhantomData,
        }
    }
}

impl<F, A, B> Future for MakeSvc<F, A, B>
where
    F: Future,
    F::Item: Discover,
    <F::Item as Discover>::Service: svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Payload,
    B: Payload,
    Balance<PeakEwmaDiscover<F::Item, PendingUntilFirstData>, http::Request<A>>:
        svc::Service<http::Request<A>>,
{
    type Item = Balance<PeakEwmaDiscover<F::Item, PendingUntilFirstData>, http::Request<A>>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let discover = try_ready!(self.inner.poll());
        let instrument = PendingUntilFirstData::default();
        let loaded = PeakEwmaDiscover::new(discover, self.default_rtt, self.decay, instrument);
        let balance = Balance::new(loaded, self.rng.clone());
        Ok(Async::Ready(balance))
    }
}

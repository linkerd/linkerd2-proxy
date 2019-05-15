extern crate hyper_balance;
extern crate tower_balance;
extern crate tower_discover;

use std::{error::Error, fmt, marker::PhantomData, time::Duration};

use futures::{future, Async, Future, Poll};
use hyper::body::Payload;

use self::tower_discover::Discover;

pub use self::hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
pub use self::tower_balance::{
    choose::PowerOfTwoChoices, load::WithPeakEwma, Balance, HasWeight, Weight, WithWeighted,
};

use http;
use proxy::{
    self,
    http::fallback,
    resolve::{EndpointStatus, HasEndpointStatus},
};
use svc;

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct Layer<A, B> {
    decay: Duration,
    default_rtt: Duration,
    _marker: PhantomData<fn(A) -> B>,
}

/// Resolves `T` typed targets to balance requests over `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct MakeSvc<M, A, B> {
    decay: Duration,
    default_rtt: Duration,
    inner: M,
    _marker: PhantomData<fn(A) -> B>,
}

#[derive(Debug)]
pub struct Service<S> {
    balance: S,
    status: EndpointStatus,
}

#[derive(Debug)]
pub struct NoEndpoints;

// === impl Layer ===

pub fn layer<A, B>(default_rtt: Duration, decay: Duration) -> Layer<A, B> {
    Layer {
        decay,
        default_rtt,
        _marker: PhantomData,
    }
}

impl<A, B> Clone for Layer<A, B> {
    fn clone(&self) -> Self {
        Layer {
            decay: self.decay,
            default_rtt: self.default_rtt,
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
            _marker: PhantomData,
        }
    }
}

impl<T, M, A, B> svc::Service<T> for MakeSvc<M, A, B>
where
    M: svc::Service<T>,
    M::Response: Discover + HasEndpointStatus,
    <M::Response as Discover>::Service:
        svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Payload,
    B: Payload,
{
    type Response =
        Service<Balance<WithPeakEwma<M::Response, PendingUntilFirstData>, PowerOfTwoChoices>>;
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
            _marker: PhantomData,
        }
    }
}

impl<F, A, B> Future for MakeSvc<F, A, B>
where
    F: Future,
    F::Item: Discover + HasEndpointStatus,
    <F::Item as Discover>::Service: svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Payload,
    B: Payload,
{
    type Item = Service<Balance<WithPeakEwma<F::Item, PendingUntilFirstData>, PowerOfTwoChoices>>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let discover = try_ready!(self.inner.poll());
        let status = discover.endpoint_status();
        let instrument = PendingUntilFirstData::default();
        let loaded = WithPeakEwma::new(discover, self.default_rtt, self.decay, instrument);
        let balance = Balance::p2c(loaded);
        Ok(Async::Ready(Service { balance, status }))
    }
}

impl<S, A, B> svc::Service<http::Request<A>> for Service<S>
where
    S: svc::Service<http::Request<A>, Response = http::Response<B>, Error = proxy::Error>,
{
    type Response = http::Response<B>;
    type Error = fallback::Error<A>;
    type Future = future::Either<
        future::MapErr<S::Future, fn(proxy::Error) -> Self::Error>,
        future::FutureResult<http::Response<B>, fallback::Error<A>>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        let ready = self.balance.poll_ready().map_err(fallback::Error::from)?;
        if self.status.is_empty() {
            Ok(Async::Ready(()))
        } else {
            Ok(ready)
        }
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        // The endpoint status is updated by the Discover instance, which is
        // driven by calling `poll_ready` on the balancer.
        if self.status.is_empty() {
            future::Either::B(future::err(fallback::Error::fallback(req, NoEndpoints)))
        } else {
            future::Either::A(self.balance.call(req).map_err(From::from))
        }
    }
}

pub mod weight {
    use super::tower_balance::{HasWeight, Weight, Weighted};
    use futures::{Future, Poll};
    use svc;

    #[derive(Clone, Debug)]
    pub struct MakeSvc<M> {
        inner: M,
    }

    #[derive(Debug)]
    pub struct MakeFuture<F> {
        inner: F,
        weight: Weight,
    }

    pub fn layer<M>() -> impl svc::Layer<M, Service = MakeSvc<M>> + Copy {
        svc::layer::mk(|inner| MakeSvc { inner })
    }

    impl<T, M> svc::Service<T> for MakeSvc<M>
    where
        T: HasWeight,
        M: svc::Service<T>,
    {
        type Response = Weighted<M::Response>;
        type Error = M::Error;
        type Future = MakeFuture<M::Future>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, target: T) -> Self::Future {
            MakeFuture {
                weight: target.weight(),
                inner: self.inner.call(target),
            }
        }
    }

    impl<F> Future for MakeFuture<F>
    where
        F: Future,
    {
        type Item = Weighted<F::Item>;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let svc = try_ready!(self.inner.poll());
            Ok(Weighted::new(svc, self.weight).into())
        }
    }
}

// === impl NoEndpoints ===

impl fmt::Display for NoEndpoints {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt("load balancer has no endpoints", f)
    }
}

impl Error for NoEndpoints {}

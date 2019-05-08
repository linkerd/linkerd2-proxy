extern crate hyper_balance;
extern crate tower_balance;
extern crate tower_discover;

use std::{fmt, marker::PhantomData, time::Duration};

use futures::{future, Async, Future, Poll};
use hyper::body::Payload;
use tower::discover::Discover;

pub use self::hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
pub use self::tower_balance::{
    choose::PowerOfTwoChoices, load::WithPeakEwma, Balance, HasWeight, Weight, WithWeighted,
};

use http;
use proxy::resolve::{EndpointStatus, HasEndpointStatus};
use svc;

type Error = Box<dyn std::error::Error + Send + Sync>;

pub mod fallback;

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct Layer<A, B, D> {
    decay: Duration,
    default_rtt: Duration,
    discover: D,
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

#[derive(Clone, Debug)]
pub struct NoEndpoints {
    _p: (),
}

// === impl Layer ===

pub fn layer<A, B, D>(default_rtt: Duration, decay: Duration, discover: D) -> Layer<A, B, D> {
    Layer {
        decay,
        default_rtt,
        discover,
        _marker: PhantomData,
    }
}

impl<A, B, D: Clone> Clone for Layer<A, B, D> {
    fn clone(&self) -> Self {
        Layer {
            decay: self.decay,
            default_rtt: self.default_rtt,
            discover: self.discover.clone(),
            _marker: PhantomData,
        }
    }
}

impl<M, A, B, D> svc::Layer<M> for Layer<A, B, D>
where
    A: Payload,
    B: Payload,
    D: svc::Layer<M> + Clone,
{
    type Service = MakeSvc<D::Service, A, B>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            decay: self.decay,
            default_rtt: self.default_rtt,
            inner: self.discover.layer(inner),
            _marker: PhantomData,
        }
    }
}

impl<A, B, D> Layer<A, B, D> {
    pub fn with_fallback<R>(self, fallback: svc::ServiceBuilder<R>) -> fallback::Layer<R, Self, A> {
        fallback::layer(self, fallback)
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
    S: svc::Service<http::Request<A>, Response = http::Response<B>>,
    S::Error: Into<Error>,
{
    type Response = http::Response<B>;
    type Error = Error;
    type Future = future::Either<
        future::FutureResult<Self::Response, Self::Error>,
        future::MapErr<S::Future, fn(S::Error) -> Self::Error>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.balance.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        if self.status.is_empty() {
            return future::Either::A(future::err(Box::new(NoEndpoints { _p: () })));
        }
        future::Either::B(self.balance.call(req).map_err(Into::into))
    }
}

impl<S> HasEndpointStatus for Service<S> {
    fn endpoint_status(&self) -> EndpointStatus {
        self.status.clone()
    }
}

impl fmt::Display for NoEndpoints {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt("load balancer has no endpoints", f)
    }
}

impl ::std::error::Error for NoEndpoints {}

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

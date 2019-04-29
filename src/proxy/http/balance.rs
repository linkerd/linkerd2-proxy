extern crate hyper_balance;
extern crate tower_balance;
extern crate tower_discover;

use std::marker::PhantomData;
use std::time::Duration;

use futures::{Future, Poll};
use hyper::body::Payload;

use self::tower_discover::Discover;

pub use self::hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
pub use self::tower_balance::{
    choose::PowerOfTwoChoices, load::WithPeakEwma, Balance, HasWeight, Weight, WithWeighted,
};

use http;
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
    M::Response: Discover,
    <M::Response as Discover>::Service:
        svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Payload,
    B: Payload,
{
    type Response = Balance<WithPeakEwma<M::Response, PendingUntilFirstData>, PowerOfTwoChoices>;
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
    F::Item: Discover,
    <F::Item as Discover>::Service: svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Payload,
    B: Payload,
{
    type Item = Balance<WithPeakEwma<F::Item, PendingUntilFirstData>, PowerOfTwoChoices>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let discover = try_ready!(self.inner.poll());
        let instrument = PendingUntilFirstData::default();
        let loaded = WithPeakEwma::new(discover, self.default_rtt, self.decay, instrument);
        Ok(Balance::p2c(loaded).into())
    }
}

pub mod weight {
    use super::tower_balance::{HasWeight, Weight, Weighted};
    use futures::{Future, Poll};
    use svc;

    #[derive(Clone, Debug)]
    pub struct Layer(());

    #[derive(Clone, Debug)]
    pub struct MakeSvc<M> {
        inner: M,
    }

    #[derive(Debug)]
    pub struct MakeFuture<F> {
        inner: F,
        weight: Weight,
    }

    pub fn layer() -> Layer {
        Layer(())
    }

    impl<M> svc::Layer<M> for Layer {
        type Service = MakeSvc<M>;

        fn layer(&self, inner: M) -> Self::Service {
            Self::Service { inner }
        }
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

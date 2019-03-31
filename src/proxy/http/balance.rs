extern crate hyper_balance;
extern crate tower_balance;
extern crate tower_discover;

use self::tower_discover::Discover;
use hyper::body::Payload;
use std::marker::PhantomData;
use std::time::Duration;

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
pub struct Stack<M, A, B> {
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

impl<T, M, A, B> svc::Layer<T, T, M> for Layer<A, B>
where
    M: svc::Stack<T> + Clone,
    M::Value: Discover,
    <M::Value as Discover>::Service:
        HasWeight + svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Payload,
    B: Payload,
{
    type Value = <Stack<M, A, B> as svc::Stack<T>>::Value;
    type Error = <Stack<M, A, B> as svc::Stack<T>>::Error;
    type Stack = Stack<M, A, B>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            decay: self.decay,
            default_rtt: self.default_rtt,
            inner,
            _marker: PhantomData,
        }
    }
}

// === impl Stack ===

impl<M: Clone, A, B> Clone for Stack<M, A, B> {
    fn clone(&self) -> Self {
        Stack {
            decay: self.decay,
            default_rtt: self.default_rtt,
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T, M, A, B> svc::Stack<T> for Stack<M, A, B>
where
    M: svc::Stack<T> + Clone,
    M::Value: Discover,
    <M::Value as Discover>::Service:
        HasWeight + svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Payload,
    B: Payload,
{
    type Value =
        Balance<WithWeighted<WithPeakEwma<M::Value, PendingUntilFirstData>>, PowerOfTwoChoices>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let discover = self.inner.make(target)?;
        let instrument = PendingUntilFirstData::default();
        let load = WithPeakEwma::new(discover, self.default_rtt, self.decay, instrument);
        Ok(Balance::p2c(WithWeighted::from(load)))
    }
}

pub mod weight {
    use super::tower_balance::{HasWeight, Weighted};
    use svc;

    #[derive(Clone, Debug)]
    pub struct Layer(());

    #[derive(Clone, Debug)]
    pub struct Stack<M> {
        inner: M,
    }

    pub fn layer() -> Layer {
        Layer(())
    }

    impl<T, M> svc::Layer<T, T, M> for Layer
    where
        M: svc::Stack<T>,
        T: HasWeight,
    {
        type Value = <Stack<M> as svc::Stack<T>>::Value;
        type Error = <Stack<M> as svc::Stack<T>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack { inner }
        }
    }

    impl<T, M> svc::Stack<T> for Stack<M>
    where
        M: svc::Stack<T>,
        T: HasWeight,
    {
        type Value = Weighted<M::Value>;
        type Error = M::Error;

        fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
            let inner = self.inner.make(&target)?;
            Ok(Weighted::new(inner, target.weight()))
        }
    }
}

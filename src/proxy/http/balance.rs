extern crate tower_balance;
extern crate tower_discover;
extern crate tower_h2_balance;

use std::marker::PhantomData;
use std::time::Duration;
use self::tower_discover::Discover;

pub use self::tower_balance::{choose::PowerOfTwoChoices, load::WithPeakEwma, Balance};
pub use self::tower_h2_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};

use http;
use svc;
use tower_h2::Body;

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct Layer<A, B> {
    decay: Duration,
    _marker: PhantomData<fn(A) -> B>,
}

/// Resolves `T` typed targets to balance requests over `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct Stack<M, A, B> {
    decay: Duration,
    inner: M,
    _marker: PhantomData<fn(A) -> B>,
}

// === impl Layer ===

pub fn layer<A, B>() -> Layer<A, B> {
    Layer {
        decay: Layer::DEFAULT_DECAY,
        _marker: PhantomData,
    }
}

impl Layer<(), ()> {
    const DEFAULT_DECAY: Duration = Duration::from_secs(10);

    // pub fn with_decay(self, decay: Duration) -> Self {
    //     Self {
    //         decay,
    //         .. self
    //     }
    // }
}

impl<A, B> Clone for Layer<A, B> {
    fn clone(&self) -> Self {
        Layer {
            decay: self.decay,
            _marker: PhantomData,
        }
    }
}

impl<T, M, A, B> svc::Layer<T, T, M> for Layer<A, B>
where
    M: svc::Stack<T> + Clone,
    M::Value: Discover,
    <M::Value as Discover>::Service: svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Body,
    B: Body,
{
    type Value = <Stack<M, A, B> as svc::Stack<T>>::Value;
    type Error = <Stack<M, A, B> as svc::Stack<T>>::Error;
    type Stack = Stack<M, A, B>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            decay: self.decay,
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
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T, M, A, B> svc::Stack<T> for Stack<M, A, B>
where
    M: svc::Stack<T> + Clone,
    M::Value: Discover,
    <M::Value as Discover>::Service: svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Body,
    B: Body,
{
    type Value = Balance<WithPeakEwma<M::Value, PendingUntilFirstData>, PowerOfTwoChoices>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let discover = self.inner.make(target)?;
        let instrument = PendingUntilFirstData::default();
        let loaded = WithPeakEwma::new(discover, self.decay, instrument);
        Ok(Balance::p2c(loaded))
    }
}

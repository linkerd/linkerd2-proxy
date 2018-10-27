extern crate tower_balance;
extern crate tower_discover;
extern crate tower_h2_balance;

use http;
use std::time::Duration;
use self::tower_discover::Discover;
use tower_h2::Body;

pub use self::tower_balance::{choose::PowerOfTwoChoices, load::WithPeakEwma, Balance};
pub use self::tower_h2_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};

use svc;

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Clone, Debug)]
pub struct Layer {
    decay: Duration,
}

/// Resolves `T` typed targets to balance requests over `M`-typed endpoint stacks.
#[derive(Clone, Debug)]
pub struct Stack<M> {
    decay: Duration,
    inner: M,
}

// === impl Layer ===

pub fn layer() -> Layer {
    Layer {
        decay: Layer::DEFAULT_DECAY,
    }
}

impl Layer {
    const DEFAULT_DECAY: Duration = Duration::from_secs(10);

    // pub fn with_decay(self, decay: Duration) -> Self {
    //     Self {
    //         decay,
    //         .. self
    //     }
    // }
}

impl<T, M, A, B> svc::Layer<T, T, M> for Layer
where
    M: svc::Stack<T> + Clone,
    M::Value: Discover<Request = http::Request<A>, Response = http::Response<B>>,
    A: Body,
    B: Body,
{
    type Value = <Stack<M> as svc::Stack<T>>::Value;
    type Error = <Stack<M> as svc::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            decay: self.decay,
            inner,
        }
    }
}

// === impl Stack ===

impl<T, M, A, B> svc::Stack<T> for Stack<M>
where
    M: svc::Stack<T> + Clone,
    M::Value: Discover<Request = http::Request<A>, Response = http::Response<B>>,
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

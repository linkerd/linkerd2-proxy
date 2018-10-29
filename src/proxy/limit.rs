extern crate tower_in_flight_limit;

use std::fmt;

pub use self::tower_in_flight_limit::InFlightLimit;

use svc;

/// Wraps `Service` stacks with an `InFlightLimit`.
#[derive(Clone, Debug)]
pub struct Layer {
    max_in_flight: usize,
}

/// Produces `Services` wrapped with an `InFlightLimit`.
#[derive(Clone, Debug)]
pub struct Stack<M> {
    max_in_flight: usize,
    inner: M,
}

// === impl Layer ===

pub fn layer(max_in_flight: usize) -> Layer {
    Layer { max_in_flight }
}

impl<T, M> svc::Layer<T, T, M> for Layer
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
    M::Value: svc::Service + Send + 'static,
    <M::Value as svc::Service>::Request: Send,
    <M::Value as svc::Service>::Future: Send,
{
    type Value = <Stack<M> as svc::Stack<T>>::Value;
    type Error = <Stack<M> as svc::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            max_in_flight: self.max_in_flight,
        }
    }
}

// === impl Stack ===

impl<T, M> svc::Stack<T> for Stack<M>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
    M::Value: svc::Service + Send + 'static,
    <M::Value as svc::Service>::Request: Send,
    <M::Value as svc::Service>::Future: Send,
{
    type Value = InFlightLimit<M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target)?;
        Ok(InFlightLimit::new(inner, self.max_in_flight))
    }
}

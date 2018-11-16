extern crate tower_in_flight_limit;

use std::{fmt, marker::PhantomData};

pub use self::tower_in_flight_limit::InFlightLimit;

use svc;

/// Wraps `Service` stacks with an `InFlightLimit`.
#[derive(Debug)]
pub struct Layer<Req> {
    max_in_flight: usize,
    _marker: PhantomData<fn(Req)>,
}

/// Produces `Services` wrapped with an `InFlightLimit`.
#[derive(Debug)]
pub struct Stack<M, Req> {
    inner: M,
    max_in_flight: usize,
    _marker: PhantomData<fn(Req)>,
}

// === impl Layer ===

pub fn layer<Req>(max_in_flight: usize) -> Layer<Req> {
    Layer {
        max_in_flight,
        _marker: PhantomData,
    }
}

impl<Req> Clone for Layer<Req> {
    fn clone(&self) -> Self {
        Layer {
            max_in_flight: self.max_in_flight,
            _marker: PhantomData,
        }
    }
}

impl<T, M, Req> svc::Layer<T, T, M> for Layer<Req>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
    M::Value: svc::Service<Req>,
{
    type Value = <Stack<M, Req> as svc::Stack<T>>::Value;
    type Error = <Stack<M, Req> as svc::Stack<T>>::Error;
    type Stack = Stack<M, Req>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            max_in_flight: self.max_in_flight,
            _marker: PhantomData,
        }
    }
}

// === impl Stack ===

impl<M: Clone, Req> Clone for Stack<M, Req> {
    fn clone(&self) -> Self {
        Stack {
            inner: self.inner.clone(),
            max_in_flight: self.max_in_flight,
            _marker: PhantomData,
        }
    }
}

impl<T, M, Req> svc::Stack<T> for Stack<M, Req>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
    M::Value: svc::Service<Req>,
{
    type Value = InFlightLimit<M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target)?;
        Ok(InFlightLimit::new(inner, self.max_in_flight))
    }
}

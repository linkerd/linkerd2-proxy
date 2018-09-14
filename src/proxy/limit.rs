extern crate tower_in_flight_limit;

use std::fmt;
use std::marker::PhantomData;

pub use self::tower_in_flight_limit::InFlightLimit;

use svc;

/// Wraps `Service` stacks with an `InFlightLimit`.
#[derive(Debug)]
pub struct Layer<T, M> {
    max_in_flight: usize,
    _p: PhantomData<fn() -> (T, M)>
}

/// Produces `Services` wrapped with an `InFlightLimit`.
#[derive(Debug)]
pub struct Stack<T, M> {
    max_in_flight: usize,
    inner: M,
    _p: PhantomData<fn() -> T>
}

impl<T, M> Layer<T, M> {
    pub fn new(max_in_flight: usize) -> Self {
        Layer {
            max_in_flight,
            _p: PhantomData
        }
    }
}

impl<T, M> Clone for Layer<T, M> {
    fn clone(&self) -> Self {
        Self::new(self.max_in_flight)
    }
}

impl<T, M> svc::Layer<T, T, M> for Layer<T, M>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
    M::Value: svc::Service + Send + 'static,
    <M::Value as svc::Service>::Request: Send,
    <M::Value as svc::Service>::Future: Send,
{
    type Value = <Stack<T, M> as svc::Stack<T>>::Value;
    type Error = <Stack<T, M> as svc::Stack<T>>::Error;
    type Stack = Stack<T, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            max_in_flight: self.max_in_flight,
            _p: PhantomData
        }
    }
}

impl<T, M: Clone> Clone for Stack<T, M> {
    fn clone(&self) -> Self {
        Self {
            max_in_flight: self.max_in_flight,
            inner: self.inner.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, M> svc::Stack<T> for Stack<T, M>
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

use futures::{future, Poll};
use svc;

use super::Stack;

/// Implements `MakeService` using a `Stack` of `Service`.
#[derive(Clone, Debug)]
pub struct StackMakeService<T, M: Stack<T>> {
    make: M,
    target: T,
}

impl<T, M> StackMakeService<T, M>
where
    M: Stack<T>,
{
    pub fn new(make: M, target: T) -> Self {
        Self { make, target }
    }
}

impl<T, M> svc::Service<()> for StackMakeService<T, M>
where
    M: Stack<T>,
{
    type Response = M::Value;
    type Error = M::Error;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, _target: ()) -> Self::Future {
        future::result(self.make.make(&self.target))
    }
}

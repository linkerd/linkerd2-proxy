use futures::{future, Poll};
use svc;

/// Creates a stack layer that also implements `Service<T>`.
#[derive(Clone, Debug)]
pub struct Layer;

#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
}

// === impl Layer ===

pub fn layer() -> Layer {
    Layer
}

impl<T, M> super::Layer<T, T, M> for Layer
where
    M: super::Stack<T>,
{
    type Value = <Stack<M> as super::Stack<T>>::Value;
    type Error = <Stack<M> as super::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack { inner }
    }
}

// === impl Stack ===

impl<T, M> super::Stack<T> for Stack<M>
where
    M: super::Stack<T>
{
    type Value = M::Value;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        self.inner.make(target)
    }
}

impl<T, M> svc::Service<T> for Stack<M>
where
    M: super::Stack<T>,
{
    type Response = M::Value;
    type Error = M::Error;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, target: T) -> Self::Future {
        future::result(self.inner.make(&target))
    }
}

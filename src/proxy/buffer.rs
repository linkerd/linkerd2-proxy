extern crate tower_buffer;

use std::{error, fmt, marker::PhantomData};

pub use self::tower_buffer::Buffer;

use logging;
use svc;

/// Wraps `Service` stacks with a `Buffer`.
#[derive(Debug)]
pub struct Layer<Req> {
    capacity: usize,
    _marker: PhantomData<fn(Req)>,
}

/// Produces `Service`s wrapped with a `Buffer`
#[derive(Debug)]
pub struct Stack<M, Req> {
    capacity: usize,
    inner: M,
    _marker: PhantomData<fn(Req)>,
}

type Error = Box<dyn error::Error + Send + Sync>;

// === impl Layer ===

pub fn layer<Req>(capacity: usize) -> Layer<Req> {
    Layer {
        capacity,
        _marker: PhantomData,
    }
}

impl<Req> Clone for Layer<Req> {
    fn clone(&self) -> Self {
        Layer {
            capacity: self.capacity,
            _marker: PhantomData,
        }
    }
}

impl<T, M, Req> svc::Layer<T, T, M> for Layer<Req>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
    M::Value: svc::Service<Req> + Send + 'static,
    Req: Send + 'static,
    <M::Value as svc::Service<Req>>::Future: Send,
    <M::Value as svc::Service<Req>>::Error: Send + Sync,
    Error: From<M::Error> + From<<M::Value as svc::Service<Req>>::Error>,
{
    type Value = <Stack<M, Req> as svc::Stack<T>>::Value;
    type Error = <Stack<M, Req> as svc::Stack<T>>::Error;
    type Stack = Stack<M, Req>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            capacity: self.capacity,
            inner,
            _marker: PhantomData,
        }
    }
}

// === impl Stack ===

impl<M: Clone, Req> Clone for Stack<M, Req> {
    fn clone(&self) -> Self {
        Stack {
            capacity: self.capacity,
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T, M, Req> svc::Stack<T> for Stack<M, Req>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
    M::Value: svc::Service<Req> + Send + 'static,
    Req: Send + 'static,
    <M::Value as svc::Service<Req>>::Future: Send,
    <M::Value as svc::Service<Req>>::Error: Send + Sync,
    Error: From<M::Error> + From<<M::Value as svc::Service<Req>>::Error>,
{
    type Value = Buffer<M::Value, Req>;
    type Error = Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target).map_err(Error::from)?;
        let mut executor = logging::context_executor(target.clone());
        Buffer::with_executor(inner, self.capacity, &mut executor)
    }
}

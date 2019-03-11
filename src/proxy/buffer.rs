extern crate tower_buffer;

use std::{error, fmt, marker::PhantomData};

pub use self::tower_buffer::{Buffer};

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

pub enum Error<M> {
    Stack(M),
    Spawn,
}

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
    <M::Value as svc::Service<Req>>::Future: Send,
    <M::Value as svc::Service<Req>>::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    Req: Send + 'static,
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
    <M::Value as svc::Service<Req>>::Future: Send,
    <M::Value as svc::Service<Req>>::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    Req: Send + 'static,
{
    type Value = Buffer<M::Value, Req>;
    type Error = Error<M::Error>;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target).map_err(Error::Stack)?;
        let executor = logging::context_executor(target.clone());
        Buffer::with_executor(inner, self.capacity, &executor).map_err(|_| Error::Spawn)
    }
}

// === impl Error ===

impl<M: fmt::Debug> fmt::Debug for Error<M> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Stack(e) => fmt.debug_tuple("buffer::Error::Stack").field(e).finish(),
            Error::Spawn => fmt.debug_tuple("buffer::Error::Spawn").finish(),
        }
    }
}

impl<M: fmt::Display> fmt::Display for Error<M> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Stack(e) => fmt::Display::fmt(e, fmt),
            Error::Spawn => write!(fmt, "Stack built without an executor"),
        }
    }
}

impl<M: error::Error> error::Error for Error<M> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Stack(e) => e.source(),
            Error::Spawn => None,
        }
    }
}

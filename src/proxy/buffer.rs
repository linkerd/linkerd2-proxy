extern crate tower_buffer;

use std::fmt;
use std::marker::PhantomData;

pub use self::tower_buffer::{Buffer, SpawnError};

use logging;
use svc;

/// Wraps `Service` stacks with a `Buffer`.
#[derive(Debug)]
pub struct Layer<T, M>(PhantomData<fn() -> (T, M)>);

/// Produces `Service`s wrapped with a `Buffer`
#[derive(Debug)]
pub struct Stack<T, M> {
    inner: M,
    _p: PhantomData<fn() -> T>,
}

pub enum Error<M, S> {
    Stack(M),
    Spawn(SpawnError<S>),
}

// === impl Layer ===

impl<T, M> Layer<T, M> {
    pub fn new() -> Self {
        Layer(PhantomData)
    }
}

impl<T, M> Clone for Layer<T, M> {
    fn clone(&self) -> Self {
        Self::new()
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
            _p: PhantomData,
        }
    }
}

// === impl Stack ===

impl<T, M: Clone> Clone for Stack<T, M> {
    fn clone(&self) -> Self {
        Self {
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
    type Value = Buffer<M::Value>;
    type Error = Error<M::Error, M::Value>;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target).map_err(Error::Stack)?;
        let executor = logging::context_executor(target.clone());
        Buffer::new(inner, &executor).map_err(Error::Spawn)
    }
}

// === impl Error ===

impl<M: fmt::Debug, S> fmt::Debug for Error<M, S> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Stack(e) => fmt.debug_tuple("buffer::Error::Stack").field(e).finish(),
            Error::Spawn(_) => fmt.debug_tuple("buffer::Error::Spawn").finish(),
        }
    }
}

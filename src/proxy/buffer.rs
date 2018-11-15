extern crate tower_buffer;

use std::{error, fmt, marker::PhantomData};

pub use self::tower_buffer::{Buffer, Error as ServiceError, SpawnError};

use logging;
use svc;

/// Wraps `Service` stacks with a `Buffer`.
#[derive(Debug)]
pub struct Layer<Req>(PhantomData<fn(Req)>);

/// Produces `Service`s wrapped with a `Buffer`
#[derive(Debug)]
pub struct Stack<M, Req> {
    inner: M,
    _marker: PhantomData<fn(Req)>,
}

pub enum Error<M, S> {
    Stack(M),
    Spawn(SpawnError<S>),
}

// === impl Layer ===

pub fn layer<Req>() -> Layer<Req> {
    Layer(PhantomData)
}

impl<Req> Clone for Layer<Req> {
    fn clone(&self) -> Self {
        Layer(PhantomData)
    }
}

impl<T, M, Req> svc::Layer<T, T, M> for Layer<Req>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
    M::Value: svc::Service<Req> + Send + 'static,
    <M::Value as svc::Service<Req>>::Future: Send,
    Req: Send + 'static,
{
    type Value = <Stack<M, Req> as svc::Stack<T>>::Value;
    type Error = <Stack<M, Req> as svc::Stack<T>>::Error;
    type Stack = Stack<M, Req>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            _marker: PhantomData,
        }
    }
}

// === impl Stack ===

impl<M: Clone, Req> Clone for Stack<M, Req> {
    fn clone(&self) -> Self {
        Stack {
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
    Req: Send + 'static,
{
    type Value = Buffer<M::Value, Req>;
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

impl<M: fmt::Display, S> fmt::Display for Error<M, S> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Stack(e) => fmt::Display::fmt(e, fmt),
            Error::Spawn(_) => write!(fmt, "Stack built without an executor"),
        }
    }
}

impl<M: error::Error, S> error::Error for Error<M, S> {
    fn cause(&self) -> Option<&error::Error> {
        match self {
            Error::Stack(e) => e.cause(),
            Error::Spawn(_) => None,
        }
    }
}

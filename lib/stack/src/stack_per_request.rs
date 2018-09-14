#![allow(dead_code)]

use futures::Poll;
use std::fmt;
use std::marker::PhantomData;

use svc;

/// A `Layer` produces a `Service` `Stack` that creates a new service for each
/// request.
#[derive(Debug)]
pub struct Layer<T, M>(PhantomData<fn() -> (T, M)>);

/// A `Stack` that builds a new `Service` for each request it serves.
#[derive(Debug)]
pub struct Stack<T, M: super::Stack<T>> {
    inner: M,
    _p: PhantomData<fn() -> T>,
}

/// A `Service` that uses a new inner service for each request.
 ///
/// `Service` does not handle any underlying errors and it is expected that an
/// instance will not be used after an error is returned.
pub struct Service<T, M: super::Stack<T>> {
    // When `poll_ready` is called, the _next_ service to be used may be bound
    // ahead-of-time. This stack is used only to serve the next request to this
    // service.
    next: Option<M::Value>,
    make: StackValid<T, M>
}

/// A helper that asserts `M` can successfully build services for a specific
/// value of `T`.
struct StackValid<T, M: super::Stack<T>> {
    target: T,
    make: M,
}

// === Layer ===

impl<T, N> Layer<T, N>
where
    T: Clone,
    N: super::Stack<T> + Clone,
    N::Error: fmt::Debug,
{
    pub fn new() -> Self {
        Layer(PhantomData)
    }
}

impl<T, N> Clone for Layer<T, N>
where
    T: Clone,
    N: super::Stack<T> + Clone,
    N::Error: fmt::Debug,
{
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<T, N> super::Layer<T, T, N> for Layer<T, N>
where
    T: Clone,
    N: super::Stack<T> + Clone,
    N::Error: fmt::Debug,
{
    type Value = <Stack<T, N> as super::Stack<T>>::Value;
    type Error = <Stack<T, N> as super::Stack<T>>::Error;
    type Stack = Stack<T, N>;

    fn bind(&self, inner: N) -> Self::Stack {
        Stack {
            inner,
            _p: PhantomData,
        }
    }
}

// === Stack ===

impl<T, N> Clone for Stack<T, N>
where
    T: Clone,
    N: super::Stack<T> + Clone,
    N::Error: fmt::Debug,
{
    fn clone(&self) -> Self {
        let inner = self.inner.clone();
        Stack { inner, _p: PhantomData }
    }
}

impl<T, N> super::Stack<T> for Stack<T, N>
where
    T: Clone,
    N: super::Stack<T> + Clone,
    N::Error: fmt::Debug,
{
    type Value = Service<T, N>;
    type Error = N::Error;

    fn make(&self, target: &T) -> Result<Self::Value, N::Error> {
        let next = self.inner.make(target)?;
        let valid = StackValid {
            make: self.inner.clone(),
            target: target.clone(),
        };
        Ok(Service {
            next: Some(next),
            make: valid,
        })
    }
}

// === Service ===

impl<T, N> svc::Service for Service<T, N>
where
    T: Clone,
    N: super::Stack<T> + Clone,
    N::Value: svc::Service,
    N::Error: fmt::Debug,
{
    type Request = <N::Value as svc::Service>::Request;
    type Response = <N::Value as svc::Service>::Response;
    type Error = <N::Value as svc::Service>::Error;
    type Future = <N::Value as svc::Service>::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if let Some(ref mut svc) = self.next {
            return svc.poll_ready();
        }

        trace!("poll_ready: new disposable client");
        let mut svc = self.make.make_valid();
        let ready = svc.poll_ready()?;
        self.next = Some(svc);
        Ok(ready)
    }

    fn call(&mut self, request: Self::Request) -> Self::Future {
        // If a service has already been bound in `poll_ready`, consume it.
        // Otherwise, bind a new service on-the-spot.
        self.next
            .take()
            .unwrap_or_else(|| self.make.make_valid())
            .call(request)
    }
}

// === StackValid ===

impl<T, M> StackValid<T, M>
where
    M: super::Stack<T>,
    M::Error: fmt::Debug
{
    fn make_valid(&self) -> M::Value {
        self.make
            .make(&self.target)
            .expect("make must succeed")
    }
}

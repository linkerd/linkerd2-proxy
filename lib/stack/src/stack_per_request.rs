#![allow(dead_code)]

use futures::Poll;
use std::fmt;

use svc;

pub trait ShouldStackPerRequest {
    fn should_stack_per_request(&self) -> bool;
}

/// A `Layer` produces a `Service` `Stack` that creates a new service for each
/// request.
#[derive(Clone, Debug)]
pub struct Layer();

/// A `Stack` that builds a new `Service` for each request it serves.
#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
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
    make: StackValid<T, M>,
}

/// A helper that asserts `M` can successfully build services for a specific
/// value of `T`.
#[derive(Clone, Debug)]
struct StackValid<T, M: super::Stack<T>> {
    target: T,
    make: M,
}

// === Layer ===

pub fn layer() -> Layer {
    Layer()
}

impl<T, N> super::Layer<T, T, N> for Layer
where
    T: ShouldStackPerRequest + Clone,
    N: super::Stack<T> + Clone,
    N::Error: fmt::Debug,
{
    type Value = <Stack<N> as super::Stack<T>>::Value;
    type Error = <Stack<N> as super::Stack<T>>::Error;
    type Stack = Stack<N>;

    fn bind(&self, inner: N) -> Self::Stack {
        Stack { inner }
    }
}

// === Stack ===

impl<T, N> super::Stack<T> for Stack<N>
where
    T: ShouldStackPerRequest + Clone,
    N: super::Stack<T> + Clone,
    N::Error: fmt::Debug,
{
    type Value = super::Either<Service<T, N>, N::Value>;
    type Error = N::Error;

    fn make(&self, target: &T) -> Result<Self::Value, N::Error> {
        let inner = self.inner.make(target)?;
        if target.should_stack_per_request() {
            Ok(super::Either::A(Service {
                next: Some(inner),
                make: StackValid {
                    make: self.inner.clone(),
                    target: target.clone(),
                },
            }))
        } else {
            Ok(super::Either::B(inner))
        }
    }
}

// === Service ===

impl<T, N, R> svc::Service<R> for Service<T, N>
where
    T: ShouldStackPerRequest + Clone,
    N: super::Stack<T> + Clone,
    N::Value: svc::Service<R>,
    N::Error: fmt::Debug,
{
    type Response = <N::Value as svc::Service<R>>::Response;
    type Error = <N::Value as svc::Service<R>>::Error;
    type Future = <N::Value as svc::Service<R>>::Future;

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

    fn call(&mut self, request: R) -> Self::Future {
        // If a service has already been bound in `poll_ready`, consume it.
        // Otherwise, bind a new service on-the-spot.
        self.next
            .take()
            .unwrap_or_else(|| self.make.make_valid())
            .call(request)
    }
}

impl<T, N> Clone for Service<T, N>
where
    T: ShouldStackPerRequest + Clone,
    N: super::Stack<T> + Clone,
{
    fn clone(&self) -> Self {
        Self {
            next: None,
            make: self.make.clone(),
        }
    }
}

// === StackValid ===

impl<T, M> StackValid<T, M>
where
    M: super::Stack<T>,
    M::Error: fmt::Debug,
{
    fn make_valid(&self) -> M::Value {
        self.make.make(&self.target).expect("make must succeed")
    }
}

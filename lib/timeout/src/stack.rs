use std::time::Duration;

use super::Timeout;
use futures::{Future, Poll};
use linkerd2_stack as stk;
use svc;

/// Creates a layer that *always* applies the timeout to every request.
///
/// As this is protocol-agnostic, timeouts are signaled via an error on
/// the future.
pub fn layer(timeout: Duration) -> Layer {
    Layer { timeout }
}

#[derive(Clone, Debug)]
pub struct Layer {
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
    timeout: Duration,
}

pub struct MakeFuture<F> {
    inner: F,
    timeout: Duration,
}

impl<M> stk::Layer<M> for Layer {
    type Service = Stack<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            timeout: self.timeout,
        }
    }
}

impl<T, M> svc::Service<T> for Stack<M>
where
    M: svc::Service<T>,
{
    type Response = Timeout<M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.call(target);
        MakeFuture {
            inner,
            timeout: self.timeout,
        }
    }
}

impl<F: Future> Future for MakeFuture<F> {
    type Item = Timeout<F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        Ok(Timeout::new(inner, self.timeout).into())
    }
}

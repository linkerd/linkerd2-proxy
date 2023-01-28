use crate::{NewService, Service};
use futures::future;
use linkerd_error::Infallible;
use std::task::{Context, Poll};

/// A `NewService<T>` that moves targets and inner services into a [`Thunk`].
#[derive(Clone, Debug)]
pub struct NewThunk<S> {
    inner: S,
}

/// A `Service<()>` that clones a `T`-typed target and calls an `S`-typed inner
/// service.
#[derive(Clone, Debug)]
pub struct Thunk<T, S> {
    target: T,
    inner: S,
}

/// A `Service<()>` that clones a `Rsp`-typed response for each call.
#[derive(Debug)]
pub struct ThunkClone<Rsp> {
    response: Rsp,
}

// === impl NewThunk ===

impl<S> NewThunk<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }

    pub fn layer() -> impl crate::layer::Layer<S, Service = Self> + Clone {
        crate::layer::mk(Self::new)
    }
}

impl<S: Clone, T> NewService<T> for NewThunk<S> {
    type Service = Thunk<T, S>;

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.clone();
        Thunk { inner, target }
    }
}

// === impl Thunk ===

impl<T, S> tower::Service<()> for Thunk<T, S>
where
    T: Clone,
    S: tower::Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, (): ()) -> S::Future {
        self.inner.call(self.target.clone())
    }
}

// === impl ThunkClone ===

impl<Rsp> ThunkClone<Rsp> {
    pub fn new(response: Rsp) -> Self {
        Self { response }
    }
}

impl<Rsp: Clone> Service<()> for ThunkClone<Rsp> {
    type Response = Rsp;
    type Error = Infallible;
    type Future = future::Ready<Result<Rsp, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (): ()) -> Self::Future {
        futures::future::ok(self.response.clone())
    }
}

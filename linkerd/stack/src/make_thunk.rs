use futures::future;
use linkerd_error::Infallible;
use std::task::{Context, Poll};
use tower::ServiceExt;

/// Wraps a `Service<T>` as a `Service<()>`.
///
/// Each time the service is called, the `T`-typed request is cloned and
/// issued into the inner service.
#[derive(Clone, Debug)]
pub struct MakeThunk<S> {
    inner: S,
}

#[derive(Clone, Debug)]
pub struct Thunk<S, T> {
    inner: S,
    target: T,
}

/// Un-thunks a `NewService<T>` that produces thunked `Service<()>`s back into a
/// `Service<T>`.
///
/// This type's `Service<T>` implementation calls `N::new_service` with a `T`,
/// returning a `Service<()>`, and oneshots that service in its `call` method.
///
/// Because the produced service thunk is oneshotted, the `Unthunk` service is
/// always ready.
#[derive(Clone, Debug)]
pub struct Unthunk<N> {
    new: N,
}

// === impl MakeThunk ===

impl<S> MakeThunk<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: Clone, T> super::NewService<T> for MakeThunk<S> {
    type Service = Thunk<S, T>;

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.clone();
        Thunk { inner, target }
    }
}

impl<S: Clone, T> tower::Service<T> for MakeThunk<S> {
    type Response = Thunk<S, T>;
    type Error = Infallible;
    type Future = future::Ready<Result<Thunk<S, T>, Infallible>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.clone();
        future::ok(Thunk { inner, target })
    }
}

// === impl Thunk ===

impl<S, T> tower::Service<()> for Thunk<S, T>
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

// === impl Unthunk ===

impl<N> Unthunk<N> {
    pub fn layer<T>() -> impl crate::layer::Layer<N, Service = Self> + Clone
    where
        N: crate::NewService<T> + Clone,
        N::Service: tower::Service<()>,
        Self: tower::Service<T>,
    {
        crate::layer::mk(|new| Self { new })
    }
}

impl<N, T> tower::Service<T> for Unthunk<N>
where
    N: crate::NewService<T>,
    N::Service: tower::Service<()>,
{
    type Response = <N::Service as tower::Service<()>>::Response;
    type Error = <N::Service as tower::Service<()>>::Error;
    type Future = tower::util::Oneshot<N::Service, ()>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: T) -> Self::Future {
        let svc = self.new.new_service(req);
        svc.oneshot(())
    }
}

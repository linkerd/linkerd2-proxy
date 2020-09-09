use futures::future;
use linkerd2_error::Never;
use std::task::{Context, Poll};

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

impl<S> MakeThunk<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: Clone, T> tower::Service<T> for MakeThunk<S> {
    type Response = Thunk<S, T>;
    type Error = Never;
    type Future = future::Ready<Result<Thunk<S, T>, Never>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Never>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.clone();
        future::ok(Thunk { inner, target })
    }
}

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

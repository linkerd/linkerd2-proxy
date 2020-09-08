use futures::future;
use linkerd2_error::Never;
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct MakeThunk<M> {
    make: M,
}

#[derive(Clone, Debug)]
pub struct Thunk<M, T> {
    make: M,
    target: T,
}

impl<M> MakeThunk<M> {
    pub fn new(make: M) -> Self {
        Self { make }
    }
}

impl<M: Clone, T> tower::Service<T> for MakeThunk<M> {
    type Response = Thunk<M, T>;
    type Error = Never;
    type Future = future::Ready<Result<Thunk<M, T>, Never>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Never>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let make = self.make.clone();
        future::ok(Thunk { make, target })
    }
}

impl<M, T> tower::Service<()> for Thunk<M, T>
where
    T: Clone,
    M: tower::Service<T>,
{
    type Response = M::Response;
    type Error = M::Error;
    type Future = M::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), M::Error>> {
        self.make.poll_ready(cx)
    }

    fn call(&mut self, (): ()) -> Self::Future {
        self.make.call(self.target.clone())
    }
}

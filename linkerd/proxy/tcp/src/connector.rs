use futures::future;
use linkerd2_error::Never;
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct Connector<C> {
    connect: C,
}

#[derive(Clone, Debug)]
pub struct Connect<C, T> {
    connect: C,
    target: T,
}

impl<C> Connector<C> {
    pub fn new(connect: C) -> Self {
        Self { connect }
    }
}

impl<C: Clone, T> tower::Service<T> for Connector<C> {
    type Response = Connect<C, T>;
    type Error = Never;
    type Future = future::Ready<Result<Connect<C, T>, Never>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Never>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let connect = self.connect.clone();
        future::ok(Connect { connect, target })
    }
}

impl<C, T> tower::Service<()> for Connect<C, T>
where
    T: Clone,
    C: tower::Service<T>,
{
    type Response = C::Response;
    type Error = C::Error;
    type Future = C::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), C::Error>> {
        self.connect.poll_ready(cx)
    }

    fn call(&mut self, (): ()) -> Self::Future {
        self.connect.call(self.target.clone())
    }
}

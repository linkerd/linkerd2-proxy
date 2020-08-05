use futures::{future, prelude::*};
use linkerd2_duplex::Duplex;
use linkerd2_error::Error;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};
use tower::Service;

#[derive(Clone, Debug)]
pub struct Forward<C> {
    connect: C,
}

#[derive(Clone, Debug)]
pub struct Accept<C, T> {
    connect: C,
    target: T,
}

impl<C> Forward<C> {
    pub fn new(connect: C) -> Self {
        Self { connect }
    }
}

impl<C: Clone, T> Service<T> for Forward<C> {
    type Response = Accept<C, T>;
    type Error = Error;
    type Future = future::Ready<Result<Self::Response, Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let connect = self.connect.clone();
        future::ok(Accept { connect, target })
    }
}

impl<C, T, I> Service<I> for Accept<C, T>
where
    T: Clone,
    I: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    C: tower::Service<T> + Send + 'static,
    C::Error: Into<Error>,
    C::Future: Send + 'static,
    C::Response: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    type Response = ();
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), self::Error>> {
        self.connect.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, src_io: I) -> Self::Future {
        let connect = self.connect.call(self.target.clone()).err_into::<Error>();
        Box::pin(async move {
            let dst_io = connect.await?;
            Duplex::new(src_io, dst_io).err_into::<Error>().await
        })
    }
}

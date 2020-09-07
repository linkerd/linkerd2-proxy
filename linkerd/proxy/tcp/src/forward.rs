use futures::{future, prelude::*};
use linkerd2_duplex::Duplex;
use linkerd2_error::Error;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tower::Service;

#[derive(Clone, Debug)]
pub struct Forward<M> {
    make_connect: M,
}

#[derive(Clone, Debug)]
pub struct Accept<C> {
    connect: C,
}

impl<M> Forward<M> {
    pub fn new(make_connect: M) -> Self {
        Self { make_connect }
    }
}

impl<M, T> Service<T> for Forward<M>
where
    M: Service<T>,
{
    type Response = Accept<M::Response>;
    type Error = M::Error;
    type Future = future::MapOk<M::Future, fn(M::Response) -> Accept<M::Response>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), M::Error>> {
        self.make_connect.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.make_connect
            .call(target)
            .map_ok(|connect| Accept { connect })
    }
}

impl<C, I> Service<I> for Accept<C>
where
    I: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    C: tower::Service<()> + Send + 'static,
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
        let connect = self.connect.call(()).err_into::<Error>();
        Box::pin(async move {
            let dst_io = connect.await?;
            Duplex::new(src_io, dst_io).err_into::<Error>().await
        })
    }
}

use futures::prelude::*;
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
pub struct Forward<C> {
    connect: C,
}

impl<M> Forward<M> {
    pub fn new(connect: M) -> Self {
        Self { connect }
    }
}

impl<C, I> Service<I> for Forward<C>
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
        Box::pin(
            self.connect
                .call(())
                .err_into::<Error>()
                .and_then(|dst_io| Duplex::new(src_io, dst_io).err_into::<Error>()),
        )
    }
}

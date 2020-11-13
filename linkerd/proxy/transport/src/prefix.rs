use crate::io;
use futures::prelude::*;
use linkerd2_error::Error;
use linkerd2_stack::layer;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time;
use tower::util::ServiceExt;
use tracing::{debug, trace};

#[derive(Copy, Clone)]
pub struct Prefix<S> {
    inner: S,
    minimum: usize,
    capacity: usize,
    timeout: Duration,
}

#[derive(Debug)]
pub struct ReadTimeout(());

impl<S> Prefix<S> {
    pub fn new(inner: S, minimum: usize, capacity: usize, timeout: Duration) -> Self {
        Self {
            inner,
            capacity,
            minimum,
            timeout,
        }
    }

    pub fn layer(
        minimum: usize,
        capacity: usize,
        timeout: Duration,
    ) -> impl layer::Layer<S, Service = Prefix<S>> + Clone {
        layer::mk(move |inner| Self::new(inner, minimum, capacity, timeout))
    }
}

impl<S, I> tower::Service<I> for Prefix<S>
where
    I: io::Peekable + Send + 'static,
    S: tower::Service<io::PrefixedIo<I>, Response = ()> + Clone + Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(().into()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        debug!(capacity = self.capacity, "Buffering prefix");
        let accept = self.inner.clone();
        let peek = time::timeout(self.timeout, io.peek(self.minimum, self.capacity))
            .map_err(|_| ReadTimeout(()));
        Box::pin(async move {
            let io = peek.await??;
            trace!(read = %io.prefix().len());
            accept.oneshot(io).err_into::<Error>().await?;
            Ok(())
        })
    }
}

impl std::fmt::Display for ReadTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Timed out while reading client stream prefix")
    }
}

impl std::error::Error for ReadTimeout {}

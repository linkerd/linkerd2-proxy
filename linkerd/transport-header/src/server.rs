use super::TransportHeader;
use bytes::BytesMut;
use futures::prelude::*;
use linkerd_error::Error;
use linkerd_io as io;
use linkerd_stack::{layer, NewService, Service, ServiceExt};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

#[derive(Clone, Debug, Default)]
pub struct NewTransportHeaderServer<N> {
    inner: N,
    timeout: time::Duration,
}

#[derive(Clone, Debug, Default)]
pub struct TransportHeaderServer<T, N> {
    target: T,
    inner: N,
    timeout: time::Duration,
}

impl<N> NewTransportHeaderServer<N> {
    pub fn layer(timeout: time::Duration) -> impl layer::Layer<N, Service = Self> + Copy {
        layer::mk(move |inner| Self { inner, timeout })
    }
}

impl<T, N: Clone> NewService<T> for NewTransportHeaderServer<N> {
    type Service = TransportHeaderServer<T, N>;

    fn new_service(&mut self, target: T) -> Self::Service {
        TransportHeaderServer {
            target,
            timeout: self.timeout,
            inner: self.inner.clone(),
        }
    }
}

impl<T, I, N, S> Service<I> for TransportHeaderServer<T, N>
where
    T: Clone + Send + 'static,
    I: io::AsyncRead + Send + Unpin + 'static,
    N: NewService<(TransportHeader, T), Service = S> + Clone + Send + 'static,
    S: Service<io::PrefixedIo<I>> + Send,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut io: I) -> Self::Future {
        let timeout = self.timeout;
        let target = self.target.clone();
        let mut inner = self.inner.clone();
        let mut buf = BytesMut::with_capacity(1024 * 64);
        Box::pin(async move {
            let hdr = futures::select_biased! {
                res = TransportHeader::read_prefaced(&mut io, &mut buf).fuse() => match res? {
                    Some(hdr) => hdr,
                    None => {
                        let e = io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Connection did not include a transport header"
                        );
                        return Err(e.into());
                    }
                },
                _ = time::sleep(timeout).fuse() => {
                    let e = io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Reading a transport header timed out"
                    );
                    return Err(e.into());
                }
            };

            inner
                .new_service((hdr, target))
                .oneshot(io::PrefixedIo::new(buf.freeze(), io))
                .await
                .map_err(Into::into)
        })
    }
}

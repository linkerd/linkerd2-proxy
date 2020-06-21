use futures::prelude::*;
use linkerd2_error::Error;
use linkerd2_io::{BoxedIo, Peekable};
use linkerd2_proxy_core as core;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::util::ServiceExt;

/// A strategy for detecting values out of a client transport.
pub trait Detect<T>: Clone {
    type Target;

    /// If the target can be determined by the target alone (i.e. because it's
    /// known to be a server-speaks-first target), Otherwise, the target is
    /// returned as an error.
    fn detect_before_peek(&self, target: T) -> Result<Self::Target, T>;

    /// If the target could not be determined without peeking, then used the
    /// peeked prefix to determine the protocol.
    fn detect_peeked_prefix(&self, target: T, prefix: &[u8]) -> Self::Target;
}

#[derive(Debug, Clone)]
pub struct DetectProtocolLayer<D> {
    detect: D,
    peek_capacity: usize,
}

#[derive(Debug, Clone)]
pub struct DetectProtocol<D, A> {
    detect: D,
    accept: A,
    peek_capacity: usize,
}

impl<D> DetectProtocolLayer<D> {
    const DEFAULT_CAPACITY: usize = 8192;

    pub fn new(detect: D) -> Self {
        Self {
            detect,
            peek_capacity: Self::DEFAULT_CAPACITY,
        }
    }
}

impl<D: Clone, A> tower::layer::Layer<A> for DetectProtocolLayer<D> {
    type Service = DetectProtocol<D, A>;

    fn layer(&self, accept: A) -> Self::Service {
        Self::Service {
            detect: self.detect.clone(),
            peek_capacity: self.peek_capacity,
            accept,
        }
    }
}

impl<T, D, A> tower::Service<(T, BoxedIo)> for DetectProtocol<D, A>
where
    T: Send + 'static,
    D: Detect<T> + Send + 'static,
    D::Target: Send,
    A: core::listen::Accept<(D::Target, BoxedIo)> + Send + Clone + 'static,
    A::Future: Send,
{
    type Response = A::ConnectionFuture;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<A::ConnectionFuture, Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The `accept` is cloned into the response future, so its readiness isn't important.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (target, io): (T, BoxedIo)) -> Self::Future {
        let detect = self.detect.clone();
        let capacity = self.peek_capacity;
        let detect = async move {
            // Try to detect the protocol from the target alone. This allows configuration to
            // skip prevent tryng to read from the client connection.
            let target = match detect.detect_before_peek(target) {
                Ok(detected) => return Ok((detected, io)),
                Err(t) => t,
            };

            // Otherwise, attempt to peek the client connection to determine the protocol.
            let prefixed = io.peek(capacity).await.map_err(Into::<Error>::into)?;
            let detected = detect.detect_peeked_prefix(target, prefixed.prefix().as_ref());
            Ok((detected, BoxedIo::new(prefixed)))
        };

        let mut accept = self.accept.clone().into_service();
        Box::pin(async move {
            // Await the service and protocol detection together. If either fails, the other is
            // aborted.
            let (accept, conn) =
                futures::try_join!(accept.ready_and().map_err(Into::into), detect)?;

            accept.call(conn).await.map_err(Into::into)
        })
    }
}

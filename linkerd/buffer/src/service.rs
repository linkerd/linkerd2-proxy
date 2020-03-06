use crate::error::{Closed, ServiceError};
use crate::InFlight;
use futures::{try_ready, Async, Future, Poll};
use linkerd2_error::Error;
use tokio::sync::{mpsc, oneshot, watch};

pub struct Buffer<Req, F> {
    /// Updates with the readiness state of the inner service. This allows the buffer to reliably
    /// exert backpressure and propagate errors, especially before the inner service has been
    /// initialized.
    ready: watch::Receiver<Poll<(), ServiceError>>,

    /// The queue on which in-flight requests are sent to the inner service.
    ///
    /// Because the inner service's status is propagated via `ready` watch, this is here to
    /// allow multiple services race to send a request.
    tx: mpsc::Sender<InFlight<Req, F>>,
}

pub enum ResponseFuture<F> {
    Receive(oneshot::Receiver<Result<F, Error>>),
    Respond(F),
}

// === impl Buffer ===

impl<Req, F> Buffer<Req, F> {
    pub(crate) fn new(
        tx: mpsc::Sender<InFlight<Req, F>>,
        ready: watch::Receiver<Poll<(), ServiceError>>,
    ) -> Self {
        Self { tx, ready }
    }

    /// Get the inner service's state, ensuring the Service is registered to receive updates.
    fn poll_inner(&mut self) -> Poll<(), Error> {
        // Drive the watch to NotReady to ensure we are registered to be notified.
        loop {
            match self.ready.poll_ref() {
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(Some(_))) => {}
                Err(_) | Ok(Async::Ready(None)) => return Err(Closed(()).into()),
            }
        }

        // Get the most recent state.
        self.ready.get_ref().clone().map_err(Into::into)
    }
}

impl<Req, F> tower::Service<Req> for Buffer<Req, F>
where
    F: Future,
    F::Error: Into<Error>,
{
    type Response = F::Item;
    type Error = Error;
    type Future = ResponseFuture<F>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.poll_inner() {
            Ok(Async::Ready(())) => self.tx.poll_ready().map_err(|_| Closed(()).into()),
            ready => ready,
        }
    }

    fn call(&mut self, request: Req) -> Self::Future {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(InFlight { request, tx })
            .ok()
            .expect("poll_ready must be called");
        Self::Future::Receive(rx)
    }
}

impl<Req, F> Clone for Buffer<Req, F> {
    fn clone(&self) -> Self {
        Self::new(self.tx.clone(), self.ready.clone())
    }
}

// === impl ResponseFuture ===

impl<F> Future for ResponseFuture<F>
where
    F: Future,
    F::Error: Into<Error>,
{
    type Item = F::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                ResponseFuture::Receive(ref mut rx) => {
                    let fut = try_ready!(rx.poll().map_err(|_| Error::from(Closed(()))))?;
                    ResponseFuture::Respond(fut)
                }
                ResponseFuture::Respond(ref mut fut) => return fut.poll().map_err(Into::into),
            };
        }
    }
}

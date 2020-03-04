use crate::error::{Closed, ServiceError};
use crate::InFlight;
use futures::{try_ready, Async, Future, Poll};
use linkerd2_error::Error;
use tokio::sync::{mpsc, oneshot, watch};

pub struct Buffer<Req, Rsp> {
    /// Updates with the readiness state of the inner service. This allows the buffer to reliably
    /// exert backpressure and propagate errors, especially before the inner service has been
    /// initialized.
    ready: watch::Receiver<Poll<(), ServiceError>>,

    /// The queue on which in-flight requests are sent to the inner service.
    ///
    /// Because the inner service's status is propagated via `ready` watch, this is here to
    /// accomodate for when multiple services race to send a requesto
    tx: mpsc::Sender<InFlight<Req, Rsp>>,
}

pub struct ResponseFuture<Rsp> {
    rx: oneshot::Receiver<Result<Rsp, Error>>,
}

// === impl Buffer ===

impl<Req, Rsp> Buffer<Req, Rsp> {
    pub(crate) fn new(
        tx: mpsc::Sender<InFlight<Req, Rsp>>,
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

impl<Req, Rsp> tower::Service<Req> for Buffer<Req, Rsp> {
    type Response = Rsp;
    type Error = Error;
    type Future = ResponseFuture<Rsp>;

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
        Self::Future { rx }
    }
}

impl<Req, Rsp> Clone for Buffer<Req, Rsp> {
    fn clone(&self) -> Self {
        Self::new(self.tx.clone(), self.ready.clone())
    }
}

// === impl ResponseFuture ===

impl<Rsp> Future for ResponseFuture<Rsp> {
    type Item = Rsp;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let ret = try_ready!(self.rx.poll().map_err(|_| Error::from(Closed(()))));
        ret.map(Async::Ready)
    }
}

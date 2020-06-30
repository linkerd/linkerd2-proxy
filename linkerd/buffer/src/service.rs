use crate::error::{Closed, ServiceError};
use crate::InFlight;
use linkerd2_error::Error;
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin};
use tokio::sync::{mpsc, oneshot, watch};

pub struct Buffer<Req, Rsp> {
    /// Updates with the readiness state of the inner service. This allows the buffer to reliably
    /// exert backpressure and propagate errors, especially before the inner service has been
    /// initialized.
    ready: watch::Receiver<Poll<Result<(), ServiceError>>>,

    /// The queue on which in-flight requests are sent to the inner service.
    ///
    /// Because the inner service's status is propagated via `ready` watch, this is here to
    /// allow multiple services race to send a request.
    tx: mpsc::Sender<InFlight<Req, Rsp>>,
}

// === impl Buffer ===

impl<Req, Rsp> Buffer<Req, Rsp> {
    pub(crate) fn new(
        tx: mpsc::Sender<InFlight<Req, Rsp>>,
        ready: watch::Receiver<Poll<Result<(), ServiceError>>>,
    ) -> Self {
        Self { tx, ready }
    }

    /// Get the inner service's state, ensuring the Service is registered to receive updates.
    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // Drive the watch to NotReady to ensure we are registered to be notified.
        loop {
            match self.ready.poll_recv_ref(cx) {
                Poll::Pending => break,
                Poll::Ready(Some(ready)) => match &*ready {
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err.clone().into())),
                    _ => {} // continue;
                },
                Poll::Ready(None) => return Poll::Ready(Err(Closed(()).into())),
            }
        }

        // Get the most recent state.
        self.ready.borrow().clone().map_err(Into::into)
    }
}

impl<Req, Rsp> tower::Service<Req> for Buffer<Req, Rsp>
where
    Rsp: Send + 'static,
{
    type Response = Rsp;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Rsp, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.poll_inner(cx) {
            Poll::Ready(Ok(())) => self.tx.poll_ready(cx).map_err(|_| Closed(()).into()),
            ready => ready,
        }
    }

    fn call(&mut self, request: Req) -> Self::Future {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(InFlight { request, tx })
            .ok()
            .expect("poll_ready must be called");
        Box::pin(async move { rx.await.map_err(|_| Closed(()))??.await })
    }
}

impl<Req, Rsp> Clone for Buffer<Req, Rsp> {
    fn clone(&self) -> Self {
        Self::new(self.tx.clone(), self.ready.clone())
    }
}

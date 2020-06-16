use crate::error::{Closed, ServiceError};
use crate::InFlight;
use futures::{ready, TryFuture};
use linkerd2_error::Error;
use pin_project::{pin_project, project};
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin};
use tokio::sync::{mpsc, oneshot, watch};

pub struct Buffer<Req, F> {
    /// Updates with the readiness state of the inner service. This allows the buffer to reliably
    /// exert backpressure and propagate errors, especially before the inner service has been
    /// initialized.
    ready: watch::Receiver<Poll<Result<(), ServiceError>>>,

    /// The queue on which in-flight requests are sent to the inner service.
    ///
    /// Because the inner service's status is propagated via `ready` watch, this is here to
    /// allow multiple services race to send a request.
    tx: mpsc::Sender<InFlight<Req, F>>,
}

#[pin_project]
pub struct ResponseFuture<F> {
    #[pin]
    state: State<F>,
}

#[pin_project]
enum State<F> {
    Receive(#[pin] oneshot::Receiver<Result<F, Error>>),
    Respond(#[pin] F),
}

// === impl Buffer ===

impl<Req, F> Buffer<Req, F> {
    pub(crate) fn new(
        tx: mpsc::Sender<InFlight<Req, F>>,
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

impl<Req, F> tower::Service<Req> for Buffer<Req, F>
where
    F: TryFuture,
    F::Error: Into<Error>,
{
    type Response = F::Ok;
    type Error = Error;
    type Future = ResponseFuture<F>;

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
        Self::Future {
            state: State::Receive(rx),
        }
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
    F: TryFuture,
    F::Error: Into<Error>,
{
    type Output = Result<F::Ok, Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            #[project]
            match this.state.as_mut().project() {
                State::Receive(rx) => {
                    let fut = ready!(rx.poll(cx).map_err(|_| Error::from(Closed(()))))??;
                    this.state.set(State::Respond(fut))
                }
                State::Respond(fut) => return fut.try_poll(cx).map_err(Into::into),
            };
        }
    }
}

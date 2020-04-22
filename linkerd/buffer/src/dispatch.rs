use crate::error::ServiceError;
use crate::InFlight;
use linkerd2_error::Error;
use pin_project::pin_project;
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::{mpsc, watch};
use tracing::trace;

/// A future that drives the inner service.
#[pin_project]
pub struct Dispatch<S, Req, F> {
    inner: Option<S>,
    #[pin]
    rx: mpsc::Receiver<InFlight<Req, F>>,
    ready: watch::Sender<Poll<Result<(), ServiceError>>>,
}

impl<S, Req> Dispatch<S, Req, S::Future>
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    pub(crate) fn new(
        inner: S,
        rx: mpsc::Receiver<InFlight<Req, S::Future>>,
        ready: watch::Sender<Poll<Result<(), ServiceError>>>,
    ) -> Self {
        Self {
            inner: Some(inner),
            rx,
            ready,
        }
    }
}

macro_rules! return_ready {
    () => {{
        trace!("Complete");
        return Poll::Ready(());
    }};
}

macro_rules! return_ready_if {
    ($cond:expr) => {{
        if $cond {
            return_ready!();
        }
    }};
}

impl<S, Req> Future for Dispatch<S, Req, S::Future>
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut this = self.project();
        // Complete the task when all services have dropped.
        return_ready_if!({
            let closed = this.ready.closed();
            tokio::pin!(closed);
            closed.poll(cx).is_ready()
        });

        // Drive requests from the queue to the inner service.
        loop {
            let ready = match this.inner.as_mut() {
                Some(inner) => inner.poll_ready(cx),
                None => {
                    // This is safe because ready.poll_close has returned Pending.
                    return Poll::Pending;
                }
            };

            match ready {
                // If it's not ready, wait for it..
                Poll::Pending => {
                    return_ready_if!(this.ready.broadcast(Poll::Pending).is_err());

                    trace!("Waiting for inner service");
                    return Poll::Pending;
                }

                // If the service fails, propagate the failure to all pending
                // requests and then complete.
                Poll::Ready(Err(error)) => {
                    let shared = ServiceError(Arc::new(error.into()));
                    trace!(%shared, "Inner service failed");

                    // First, notify services of the readiness change to prevent new requests from
                    // being buffered.
                    let is_active = this
                        .ready
                        .broadcast(Poll::Ready(Err(shared.clone())))
                        .is_ok();

                    // Propagate the error to all in-flight requests.
                    while let Poll::Ready(Some(InFlight { tx, .. })) = this.rx.poll_recv(cx) {
                        let _ = tx.send(Err(shared.clone().into()));
                    }

                    // Drop the inner Service to free its resources. It won't be used again.
                    let _ = this.inner.take();

                    // Ensure the task remains active until all services have observed the error.
                    return_ready_if!(!is_active);

                    // This is safe because ready.poll_close has returned Pending. The task will
                    // complete when all observes have dropped their interest in `ready`.
                    return Poll::Pending;
                }

                // If inner service can receive requests, start polling the channel.
                Poll::Ready(Ok(())) => {
                    return_ready_if!(this.ready.broadcast(Poll::Ready(Ok(()))).is_err());
                    trace!("Ready for requests");
                }
            }

            // The inner service is ready, so poll for new requests.
            match futures::ready!(this.rx.poll_recv(cx)) {
                // All senders have been dropped, complete.
                None => return_ready!(),

                // If a request was ready return it to the caller.
                Some(InFlight { request, tx }) => {
                    trace!("Dispatching a request");
                    let fut = this
                        .inner
                        .as_mut()
                        .expect("Service must not be dropped")
                        .call(request);
                    let _ = tx.send(Ok(fut));
                }
            }
        }
    }
}

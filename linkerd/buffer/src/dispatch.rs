use crate::error::ServiceError;
use crate::InFlight;
use futures::{Async, Future, Poll, Stream};
use linkerd2_error::{Error, Never};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tracing::trace;

/// A future that drives the inner service.
pub struct Dispatch<S, Req, Rsp> {
    inner: Option<S>,
    rx: mpsc::Receiver<InFlight<Req, Rsp>>,
    ready: watch::Sender<Poll<(), ServiceError>>,
}

impl<S, Req> Dispatch<S, Req, S::Response>
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    pub(crate) fn new(
        inner: S,
        rx: mpsc::Receiver<InFlight<Req, S::Response>>,
        ready: watch::Sender<Poll<(), ServiceError>>,
    ) -> Self {
        Self {
            inner: Some(inner),
            rx,
            ready,
        }
    }
}

impl<S, Req> Future for Dispatch<S, Req, S::Response>
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Item = ();
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Complete the task when all services have dropped.
        if self.ready.poll_close().expect("must not fail").is_ready() {
            return Ok(Async::Ready(()));
        }

        // Drive requests from the queue to the inner service.
        loop {
            let ready = match self.inner.as_mut() {
                Some(inner) => inner.poll_ready(),
                None => {
                    // This is safe because ready.poll_close has returned NotReady.
                    return Ok(Async::NotReady);
                }
            };

            match ready {
                // If it's not ready, wait for it..
                Ok(Async::NotReady) => {
                    if self.ready.broadcast(Ok(Async::NotReady)).is_err() {
                        return Ok(Async::Ready(()));
                    }
                    trace!("Waiting for inner service");
                    return Ok(Async::NotReady);
                }

                // If the service fails, propagate the failure to all pending
                // requests and then complete.
                Err(error) => {
                    let shared = ServiceError(Arc::new(error.into()));
                    trace!(%shared, "Inner service failed");

                    // First, notify services of the readiness change to prevent new requests from
                    // being buffered.
                    let is_active = self.ready.broadcast(Err(shared.clone())).is_ok();

                    // Propagate the error to all in-
                    while let Ok(Async::Ready(Some(InFlight { tx, .. }))) = self.rx.poll() {
                        let _ = tx.send(Err(shared.clone().into()));
                    }

                    // Ensure the task remains active until all services have observed the error.
                    return if is_active {
                        // Drop the inner Service to free its resources. It won't be used again.
                        self.inner = None;
                        // This is safe because ready.poll_close has returned NotReady.
                        Ok(Async::NotReady)
                    } else {
                        // All of the services have dropped, so complete the task.
                        Ok(Async::Ready(()))
                    };
                }

                // If inner service can receive requests, start polling the channel.
                Ok(Async::Ready(())) => {
                    if self.ready.broadcast(Ok(Async::Ready(()))).is_err() {
                        return Ok(Async::Ready(()));
                    }
                    trace!("Ready for requests");
                }
            }

            // The inner service is ready, so poll for new requests.
            match self.rx.poll() {
                Ok(Async::NotReady) => return Ok(Async::NotReady),

                // The sender has been dropped, complete.
                Err(_) | Ok(Async::Ready(None)) => return Ok(Async::Ready(())),

                // If a request was ready, spawn its response future.
                Ok(Async::Ready(Some(InFlight { request, tx }))) => {
                    trace!("Dispatching a request");
                    let inner = self.inner.as_mut().expect("Service must not be dropped");
                    tokio::spawn(inner.call(request).then(move |res| {
                        let _ = tx.send(res.map_err(Into::into));
                        Ok(())
                    }));
                }
            }
        }
    }
}

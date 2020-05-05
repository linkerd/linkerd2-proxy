use crate::error::{IdleError, ServiceError};
use crate::InFlight;
use linkerd2_error::Error;
use pin_project::pin_project;
use std::task::{Context, Poll};
use std::time::Duration;
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::{mpsc, watch};
use tokio::time::{delay_for, Delay};
use tracing::trace;

/// A future that drives the inner service.
#[pin_project]
pub struct Dispatch<S, Req, F> {
    inner: Option<S>,
    #[pin]
    rx: mpsc::Receiver<InFlight<Req, F>>,
    ready: watch::Sender<Poll<Result<(), ServiceError>>>,
    max_idle: Option<Duration>,
    current_idle: Option<Delay>,
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
        max_idle: Option<Duration>,
    ) -> Self {
        Self {
            inner: Some(inner),
            rx,
            ready,
            max_idle,
            current_idle: None,
        }
    }

    /// Sets an idle timeout if one does not exist and a `max_idle` value is set.
    fn set_idle(&mut self) {
        if let Some(timeout) = self.max_idle {
            if self.current_idle.is_none() {
                self.current_idle = Some(delay_for(timeout));
            }
        }
    }

    /// Check the idle timeout and, if it is expired, return an error.
    fn poll_idle(&mut self, cx: &mut Context<'_>) -> Poll<IdleError> {
        if let Some(idle) = self.current_idle.as_mut() {
            if idle.poll(cx).expect("timer must not fail").is_ready() {
                return Poll::Ready(IdleError(self.max_idle.unwrap()));
            }
        }
        Poll::Pending
    }

    /// Transitions the buffer to failing, notifying all consumers and pending
    /// requests of the failure.
    fn fail(&mut self, cx: &mut Context<'_>, error: impl Into<Error>) -> Result<(), ()> {
        let shared = ServiceError(Arc::new(error.into()));
        trace!(%shared, "Inner service failed");

        // First, notify services of the readiness change to prevent new requests from
        // being buffered.
        let broadcast = self.ready.broadcast(Poll::Ready(Err(shared.clone())));

        // Propagate the error to all in-flight requests.
        while let Poll::Ready(Some(InFlight { tx, .. })) = self.rx.poll_recv(cx) {
            let _ = tx.send(Poll::Ready(Err(shared.clone().into())));
        }

        // Drop the inner Service to free its resources. It won't be used again.
        self.inner = None;

        broadcast.map(|_| ()).map_err(|_| ())
    }
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

        // Complete the task when all services have dropped.
        return_ready_if!({
            // `watch::Sender::poll_close` is private in `tokio::sync`.
            let closed = this.ready.closed();
            tokio::pin!(closed);
            closed.poll(cx).is_ready()
        });

        // Drive requests from the queue to the inner service.
        loop {
            let ready = match this.inner.as_mut() {
                Some(inner) => inner.poll_ready(cx),
                None => {
                    // This is safe because ready.closed() has returned Pending.
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
                    // Ensure the task remains active until all services have observed the error.
                    return_ready_if!(self.fail(cx, error).is_err());
                    // This is safe because ready.closed() has returned Pending. The task will
                    // complete when all observes have dropped their interest in `ready`.
                    return Poll::Pending;
                }

                // If the inner service can receive requests, start polling the channel.
                Poll::Ready(Ok(())) => {
                    return_ready_if!(self.ready.broadcast(Poll::Ready(Ok(()))).is_err());
                    trace!("Ready for requests");
                }
            }

            // The inner service is ready, so poll for new requests.
            match this.rx.poll_recv(cx) {
                Poll::Pending => {
                    // There are no requests available, so track idleness.
                    this.set_idle();
                    if let Poll::Ready(error) = this.poll_idle(cx) {
                        return_ready_if!(this.fail(cx, error).is_err());
                    }
                    return Poll::Pending;
                }

                // All senders have been dropped, complete.
                Poll::Ready(None) => return_ready!(),

                // If a request was ready, dispatch it and return the future to the caller.
                Poll::Ready(Some(InFlight { request, tx })) => {
                    trace!("Dispatching a request");
                    let fut = this
                        .inner
                        .as_mut()
                        .expect("Service must not be dropped")
                        .call(request);
                    let _ = tx.send(Ok(fut));

                    let _ = this.current_idle.take();
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::time::{Duration, Instant};
    use tokio::sync::{mpsc, oneshot, watch};
    use tokio::time::delay_for;
    use tower_test::mock;

    #[tokio::test]
    async fn idle_when_unused() {
        let max_idle = Duration::from_millis(100);

        let (tx, rx) = mpsc::channel(1);
        let (ready_tx, ready_rx) = watch::channel(Ok(Async::NotReady));
        let (inner, mut handle) = mock::pair::<(), ()>();
        let mut dispatch = super::Dispatch::new(inner, rx, ready_tx, Some(max_idle));
        handle.allow(1);

        // Service ready without requests. Idle counter starts ticking.
        assert!(dispatch.poll().unwrap().is_not_ready());
        assert!(ready_rx.get_ref().as_ref().unwrap().is_ready());
        delay_for(max_idle).await;
        assert!(dispatch.poll().unwrap().is_not_ready(),);
        assert!(
            dispatch.inner.is_none(),
            "Did not drop inner service after timeout."
        );
        assert!(
            ready_rx.get_ref().is_err(),
            "Did not advertise an error to consumers."
        );
        drop(ready_rx);
        assert!(
            dispatch.poll().unwrap().is_ready(),
            "Did not complete after idle timeout."
        );
        drop((tx, handle));
    }

    #[tokio::test]
    async fn not_idle_when_unavailable() {
        let max_idle = Duration::from_millis(100);

        let (tx, rx) = mpsc::channel(1);
        let (ready_tx, ready_rx) = watch::channel(Ok(Async::NotReady));
        let (inner, mut handle) = mock::pair::<(), ()>();
        let mut dispatch = super::Dispatch::new(inner, rx, ready_tx, Some(max_idle));
        handle.allow(0);

        // Service ready without requests. Idle counter starts ticking.
        assert!(dispatch.poll().unwrap().is_not_ready());
        assert!(ready_rx.get_ref().as_ref().unwrap().is_not_ready());
        delay_for(max_idle).await;
        assert!(dispatch.poll().unwrap().is_not_ready(),);
        assert!(ready_rx.get_ref().as_ref().unwrap().is_not_ready());
        drop((tx, handle));
    }

    #[tokio::test]
    async fn idle_reset_by_request() {
        let max_idle = Duration::from_millis(100);

        let (mut tx, rx) = mpsc::channel(1);
        let (ready_tx, ready_rx) = watch::channel(Ok(Async::NotReady));
        let (inner, mut handle) = mock::pair::<(), ()>();
        let mut dispatch = super::Dispatch::new(inner, rx, ready_tx, Some(max_idle));
        handle.allow(1);

        // Service ready without requests. Idle counter starts ticking.
        assert!(dispatch.poll().unwrap().is_not_ready());
        assert!(ready_rx.get_ref().as_ref().unwrap().is_ready());
        delay_for(max_idle).await;
        // Send a request after the deadline has fired but before the
        // dispatch future is polled. Ensure that the request is admitted, resetting idleness.
        tx.try_send({
            let (tx, _rx) = oneshot::channel();
            super::InFlight { request: (), tx }
        })
        .ok()
        .expect("request not sent");

        assert!(dispatch.poll().unwrap().is_not_ready());
        assert!(
            ready_rx.get_ref().as_ref().unwrap().is_not_ready(),
            "Did not advertise readiness to consumers"
        );

        handle.allow(1);
        assert!(dispatch.poll().unwrap().is_not_ready());
        assert!(
            ready_rx.get_ref().as_ref().unwrap().is_ready(),
            "Did not advertise readiness to consumers"
        );
        assert!(dispatch.current_idle.is_some(), "Idle timeout not reset");

        delay_for(max_idle).await;
        assert!(dispatch.poll().unwrap().is_not_ready(),);
        assert!(
            dispatch.inner.is_none(),
            "Did not drop inner service after timeout."
        );
        assert!(
            ready_rx.get_ref().is_err(),
            "Did not advertise an error to consumers."
        );
        drop(ready_rx);
        assert!(
            dispatch.poll().unwrap().is_ready(),
            "Did not complete after idle timeout."
        );
        drop((tx, handle));
    }
}

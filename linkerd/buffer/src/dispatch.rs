use crate::error::{IdleError, ServiceError};
use crate::InFlight;
use futures::{Async, Future, Poll, Stream};
use linkerd2_error::{Error, Never};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::timer::Delay;
use tracing::trace;

/// A future that drives the inner service.
pub struct Dispatch<S, Req, F> {
    inner: Option<S>,
    rx: mpsc::Receiver<InFlight<Req, F>>,
    ready: watch::Sender<Poll<(), ServiceError>>,
    ready_set: bool,
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
        ready: watch::Sender<Poll<(), ServiceError>>,
        max_idle: Option<Duration>,
    ) -> Self {
        Self {
            inner: Some(inner),
            rx,
            ready,
            ready_set: false,
            max_idle,
            current_idle: None,
        }
    }

    /// Sets an idle timeout if one does not exist and a `max_idle` value is set.
    fn set_idle(&mut self) {
        if let Some(timeout) = self.max_idle {
            if self.current_idle.is_none() {
                self.current_idle = Some(Delay::new(Instant::now() + timeout));
            }
        }
    }

    /// Check the idle timeout and, if it is expired, return an error.
    fn check_idle(&mut self) -> Result<(), IdleError> {
        if let Some(idle) = self.current_idle.as_mut() {
            if idle.poll().expect("timer must not fail").is_ready() {
                return Err(IdleError(self.max_idle.unwrap()));
            }
        }
        Ok(())
    }

    /// Transitions the buffer to failing, notifying all consumers and pending
    /// requests of the failure.
    fn fail(&mut self, error: impl Into<Error>) -> Result<(), ()> {
        let shared = ServiceError(Arc::new(error.into()));
        trace!(%shared, "Inner service failed");

        // First, notify services of the readiness change to prevent new requests from
        // being buffered.
        let broadcast = self.ready.broadcast(Err(shared.clone()));

        // Propagate the error to all in-flight requests.
        while let Ok(Async::Ready(Some(InFlight { tx, .. }))) = self.rx.poll() {
            let _ = tx.send(Err(shared.clone().into()));
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
    type Item = ();
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        macro_rules! return_ready {
            () => {{
                trace!("Complete");
                return Ok(Async::Ready(()));
            }};
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
                    // `ready` stays in whatever state it's in. If we've already
                    // become ready, permit messages as long as there's buffer
                    // capacity.

                    trace!("Waiting for inner service");
                    return Ok(Async::NotReady);
                }

                // If the service fails, propagate the failure to all pending
                // requests and then complete.
                Err(error) => {
                    let _ = self.fail(error);
                    return_ready!();
                }

                // If the inner service can receive requests, start polling the channel.
                Ok(Async::Ready(())) => {
                    if !self.ready_set {
                        let _ = self.ready.broadcast(Ok(Async::Ready(())));
                        self.ready_set = true;
                    }
                    println!("Ready for requests");
                }
            }

            // The inner service is ready, so poll for new requests.
            match self.rx.poll() {
                Ok(Async::NotReady) => {
                    // There are no requests available, so track idleness.
                    self.set_idle();
                    if let Err(error) = self.check_idle() {
                        let _ = self.fail(error);
                        return_ready!();
                    }

                    return Ok(Async::NotReady);
                }

                // All senders have been dropped, complete.
                Err(_) | Ok(Async::Ready(None)) => return_ready!(),

                // If a request was ready, dispatch it and return the future to the caller.
                Ok(Async::Ready(Some(InFlight { request, tx }))) => {
                    trace!("Dispatching a request");
                    let fut = self
                        .inner
                        .as_mut()
                        .expect("Service must not be dropped")
                        .call(request);
                    let _ = tx.send(Ok(fut));

                    self.current_idle = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use futures::{future, Async, Future};
    use std::time::{Duration, Instant};
    use tokio::sync::{mpsc, oneshot, watch};
    use tokio::timer::Delay;
    use tower_test::mock;

    #[test]
    fn idle_when_unused() {
        run(|| {
            let max_idle = Duration::from_millis(100);

            let (tx, rx) = mpsc::channel(1);
            let (ready_tx, ready_rx) = watch::channel(Ok(Async::NotReady));
            let (inner, mut handle) = mock::pair::<(), ()>();
            let mut dispatch = super::Dispatch::new(inner, rx, ready_tx, Some(max_idle));
            handle.allow(1);

            // Service ready without requests. Idle counter starts ticking.
            assert!(dispatch.poll().unwrap().is_not_ready());
            assert!(ready_rx.get_ref().as_ref().unwrap().is_ready());
            Delay::new(Instant::now() + max_idle).map(move |()| {
                assert!(dispatch.poll().unwrap().is_ready());
                assert!(
                    ready_rx.get_ref().is_err(),
                    "Did not advertise an error to consumers."
                );
                drop((tx, handle));
            })
        })
    }

    #[test]
    fn not_idle_when_unavailable() {
        run(|| {
            let max_idle = Duration::from_millis(100);

            let (tx, rx) = mpsc::channel(1);
            let (ready_tx, ready_rx) = watch::channel(Ok(Async::NotReady));
            let (inner, mut handle) = mock::pair::<(), ()>();
            let mut dispatch = super::Dispatch::new(inner, rx, ready_tx, Some(max_idle));
            handle.allow(0);

            // Service ready without requests. Idle counter starts ticking.
            assert!(dispatch.poll().unwrap().is_not_ready());
            assert!(ready_rx.get_ref().as_ref().unwrap().is_not_ready());
            Delay::new(Instant::now() + max_idle).map(move |()| {
                assert!(dispatch.poll().unwrap().is_not_ready(),);
                assert!(ready_rx.get_ref().as_ref().unwrap().is_not_ready());
                drop((tx, handle));
            })
        })
    }

    #[test]
    fn idle_reset_by_request() {
        run(|| {
            let max_idle = Duration::from_millis(100);

            let (mut tx, rx) = mpsc::channel(1);
            let (ready_tx, ready_rx) = watch::channel(Ok(Async::NotReady));
            let (inner, mut handle) = mock::pair::<(), ()>();
            let mut dispatch = super::Dispatch::new(inner, rx, ready_tx, Some(max_idle));
            handle.allow(1);

            // Service ready without requests. Idle counter starts ticking.
            assert!(dispatch.poll().unwrap().is_not_ready());
            assert!(ready_rx.get_ref().as_ref().unwrap().is_ready());
            Delay::new(Instant::now() + max_idle).and_then(move |()| {
                // Send a request after the deadline has fired but before the
                // dispatch future is polled. Ensure that the request is admitted, resetting idleness.
                tx.try_send({
                    let (tx, _rx) = oneshot::channel();
                    super::InFlight { request: (), tx }
                })
                .ok()
                .expect("request not sent");

                println!("Polling while request is pending");
                assert!(dispatch.poll().unwrap().is_not_ready());
                assert!(
                    ready_rx.get_ref().as_ref().unwrap().is_ready(),
                    "Did not remain ready after initialization"
                );

                handle.allow(1);
                println!("Polling to detect inner serviec is ready");
                assert!(dispatch.poll().unwrap().is_not_ready());
                assert!(
                    ready_rx.get_ref().as_ref().unwrap().is_ready(),
                    "Did not advertise readiness to consumers"
                );
                assert!(dispatch.current_idle.is_some(), "Idle timeout not reset");

                Delay::new(Instant::now() + max_idle).map(move |()| {
                    assert!(dispatch.poll().unwrap().is_ready(),);
                    assert!(
                        dispatch.inner.is_none(),
                        "Did not drop inner service after timeout."
                    );
                    assert!(
                        ready_rx.get_ref().is_err(),
                        "Did not advertise an error to consumers."
                    );
                    drop((tx, handle));
                })
            })
        })
    }

    fn run<F, R>(f: F)
    where
        F: FnOnce() -> R + 'static,
        R: future::IntoFuture<Item = ()> + 'static,
    {
        tokio::runtime::current_thread::run(future::lazy(f).map_err(|_| panic!("Failed")));
    }
}

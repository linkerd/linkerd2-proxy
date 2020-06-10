use crate::error::{IdleError, ServiceError};
use crate::InFlight;
use futures::{future::FutureExt, select_biased};
use linkerd2_error::Error;
use std::sync::Arc;
use std::task::Poll;
use tokio::sync::{mpsc, watch};
use tower::util::ServiceExt;
use tracing::trace;

pub(crate) async fn idle(max: std::time::Duration) -> IdleError {
    tokio::time::delay_for(max).await;
    IdleError(max)
}

pub(crate) async fn run<S, Req, I>(
    mut service: S,
    mut requests: mpsc::Receiver<InFlight<Req, S::Future>>,
    ready: watch::Sender<Poll<Result<(), ServiceError>>>,
    idle: impl Fn() -> I,
) where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
    I: std::future::Future,
    I::Output: Into<Error>,
{
    // Drive requests from the queue to the inner service.
    let res = loop {
        {
            // Wait until the service becomes ready, and announce readiness to
            // the senders.
            let svc = match service.ready_and().await {
                Ok(svc) => svc,
                Err(e) => break Err(e),
            };
            let _ = ready.broadcast(Poll::Ready(Ok(())));

            // If there is a request ready *now*, we can consume the existing
            // readiness immediately.
            if let Ok(InFlight { request, tx }) = requests.try_recv() {
                trace!("Dispatching request immediately");
                let _ = tx.send(Ok(svc.call(request)));
                continue;
            }
        }

        // Otherwise, we need to wait for a request...
        select_biased! {
            req = requests.recv().fuse() => {
                match req {
                    None => break Ok(()),
                    Some(InFlight { request, tx }) => {
                        // The service needs to be driven to readiness again,
                        // since we may have been waiting a long time since
                        // it was last polled to readiness.
                        let svc = match service.ready_and().await {
                            Ok(svc) => svc,
                            Err(e) => break Err(e),
                        };
                        trace!("Dispatching request");
                        let _ = tx.send(Ok(svc.call(request)));
                    }
                }
            }

            e = idle().fuse() => {
                let error = ServiceError(Arc::new(e.into()));
                trace!(%error, "Idling out inner service");
                let _ = ready.broadcast(Poll::Ready(Err(error)));
                break Ok(());
            }
        }
    };

    if let Err(e) = res {
        let error = ServiceError(Arc::new(e.into()));
        trace!(%error, "Service failed");
        let _ = ready.broadcast(Poll::Ready(Err(error.clone())));
        while let Some(InFlight { tx, .. }) = requests.recv().await {
            let _ = tx.send(Err(error.clone().into()));
        }
    }

    trace!("Complete");
}

#[cfg(test)]
mod test {
    use super::*;
    use std::task::Poll;
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot, watch};
    use tokio::time::delay_for;
    use tokio_test::{assert_pending, assert_ready, task};
    use tower_test::mock;

    #[tokio::test]
    async fn idle_when_unused() {
        let max_idle = Duration::from_millis(100);

        let (tx, rx) = mpsc::channel(1);
        let (ready_tx, ready_rx) = watch::channel(Poll::Pending);
        let (inner, mut handle) = mock::pair::<(), ()>();
        let mut dispatch = task::spawn(run(inner, rx, ready_tx, || idle(max_idle)));
        handle.allow(1);

        // Service ready without requests. Idle counter starts ticking.
        assert_pending!(dispatch.poll());
        assert!(matches!(*ready_rx.borrow(), Poll::Ready(Ok(()))));

        delay_for(max_idle).await;

        assert_ready!(dispatch.poll());
        assert!(
            matches!(*ready_rx.borrow(), Poll::Ready(Err(_))),
            "Did not advertise an error to consumers."
        );
        drop((tx, handle));
    }

    #[tokio::test]
    async fn not_idle_when_pending() {
        let max_idle = Duration::from_millis(100);

        let (tx, rx) = mpsc::channel(1);
        let (ready_tx, ready_rx) = watch::channel(Poll::Pending);
        let (inner, mut handle) = mock::pair::<(), ()>();
        let mut dispatch = task::spawn(run(inner, rx, ready_tx, || idle(max_idle)));
        handle.allow(0);

        // Service ready without requests. Idle counter starts ticking.
        assert_pending!(dispatch.poll());
        assert!(ready_rx.borrow().is_pending());

        delay_for(max_idle).await;

        assert_pending!(dispatch.poll());
        assert!(ready_rx.borrow().is_pending());
        drop(tx);
    }

    #[tokio::test]
    async fn idle_reset_by_request() {
        let max_idle = Duration::from_millis(100);

        let (mut tx, rx) = mpsc::channel(1);
        let (ready_tx, ready_rx) = watch::channel(Poll::Pending);
        let (inner, mut handle) = mock::pair::<(), ()>();
        let mut dispatch = task::spawn(run(inner, rx, ready_tx, || idle(max_idle)));
        handle.allow(1);

        // Service ready without requests. Idle counter starts ticking.
        assert_pending!(dispatch.poll());
        assert!(ready_rx.borrow().is_ready());
        delay_for(max_idle).await;

        // Send a request after the deadline has fired but before the
        // dispatch future is polled. Ensure that the request is admitted, resetting idleness.
        tx.try_send({
            let (tx, _rx) = oneshot::channel();
            super::InFlight { request: (), tx }
        })
        .ok()
        .expect("request not sent");

        assert_pending!(dispatch.poll());
        assert!(
            ready_rx.borrow().is_ready(),
            "Did not advertise readiness to consumers"
        );

        handle.allow(1);
        assert_pending!(dispatch.poll());
        assert!(
            ready_rx.borrow().is_ready(),
            "Did not advertise readiness to consumers"
        );

        delay_for(max_idle).await;

        assert_ready!(dispatch.poll());
        assert!(
            matches!(*ready_rx.borrow(), Poll::Ready(Err(_))),
            "Did not advertise an error to consumers."
        );
        drop(ready_rx);
        drop((tx, handle));
    }
}

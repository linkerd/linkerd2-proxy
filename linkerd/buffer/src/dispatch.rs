use crate::error::{IdleError, ServiceError};
use crate::InFlight;
use futures::{prelude::*, select_biased};
use linkerd2_error::Error;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower::util::ServiceExt;
use tracing::trace;

pub(crate) async fn idle(max: std::time::Duration) -> IdleError {
    tokio::time::delay_for(max).await;
    IdleError(max)
}

pub(crate) async fn run<S, Req, I>(
    mut service: S,
    mut requests: mpsc::Receiver<InFlight<Req, S::Response>>,
    idle: impl Fn() -> I,
) where
    S: tower::Service<Req>,
    S::Future: Send + 'static,
    S::Error: Into<Error>,
    I: std::future::Future,
    I::Output: Into<Error>,
{
    // Drive requests from the queue to the inner service.
    loop {
        select_biased! {
            req = requests.recv().fuse() => {
                match req {
                    None => return,
                    Some(InFlight { request, tx }) => {
                       match service.ready_and().await {
                            Ok(svc) => {
                                trace!("Dispatching request");
                                let _ = tx.send(Ok(Box::pin(svc.call(request).err_into::<Error>())));
                            }
                            Err(e) =>{
                                let error = ServiceError(Arc::new(e.into()));
                                trace!(%error, "Service failed");
                                // Fail this request.
                                let _ = tx.send(Err(error.clone().into()));
                                // Drain the queue and fail all remaining requests.
                                while let Some(InFlight { tx, .. }) = requests.recv().await {
                                    let _ = tx.send(Err(error.clone().into()));
                                }
                                return;
                            }
                        };

                    }
                }
            }

            e = idle().fuse() => {
                let error = ServiceError(Arc::new(e.into()));
                trace!(%error, "Idling out inner service");
                return;
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::delay_for;
    use tokio_test::{assert_pending, assert_ready, task};
    use tower_test::mock;

    #[tokio::test]
    async fn idle_when_unused() {
        let max_idle = Duration::from_millis(100);

        let (tx, rx) = mpsc::channel(1);
        let (inner, mut handle) = mock::pair::<(), ()>();
        let mut dispatch = task::spawn(run(inner, rx, || idle(max_idle)));
        handle.allow(1);

        // Service ready without requests. Idle counter starts ticking.
        assert_pending!(dispatch.poll());

        delay_for(max_idle).await;

        assert_ready!(dispatch.poll());
        drop((tx, handle));
    }

    #[tokio::test]
    async fn idle_reset_by_request() {
        let max_idle = Duration::from_millis(100);

        let (mut tx, rx) = mpsc::channel(1);
        let (inner, mut handle) = mock::pair::<(), ()>();
        let mut dispatch = task::spawn(run(inner, rx, || idle(max_idle)));
        handle.allow(1);

        // Service ready without requests. Idle counter starts ticking.
        assert_pending!(dispatch.poll());
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

        handle.allow(1);
        assert_pending!(dispatch.poll());

        delay_for(max_idle).await;

        assert_ready!(dispatch.poll());
        drop((tx, handle));
    }
}

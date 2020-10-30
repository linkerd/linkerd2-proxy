use crate::error::{IdleError, ServiceError};
use crate::InFlight;
use futures::{prelude::*, select_biased};
use linkerd2_error::Error;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tower::util::ServiceExt;
use tracing::trace;

pub(crate) async fn idle(max: std::time::Duration) -> IdleError {
    tokio::time::sleep(max).await;
    IdleError(max)
}

pub(crate) async fn run<S, Req, I>(
    mut service: S,
    mut requests: mpsc::UnboundedReceiver<InFlight<Req, S::Response>>,
    semaphore: Arc<Semaphore>,
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
                    Some(InFlight { request, tx, .. }) => {
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
                                break;
                            }
                        };
                    }
                }
            }

            e = idle().fuse() => {
                let error = ServiceError(Arc::new(e.into()));
                trace!(%error, "Idling out inner service");
                break;
            }
        }
    }

    // Close the buffer by releasing any senders waiting on channel capacity.
    // If more than `usize::MAX >> 3` permits are added to the semaphore, it
    // will panic.
    const MAX: usize = std::usize::MAX >> 4;
    semaphore.add_permits(MAX - semaphore.available_permits());
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::sleep;
    use tokio_test::{assert_pending, assert_ready, task};
    use tower_test::mock;

    #[tokio::test]
    async fn idle_when_unused() {
        let max_idle = Duration::from_millis(100);

        let semaphore = Arc::new(Semaphore::new(1));
        let (tx, rx) = mpsc::unbounded_channel();
        let (inner, mut handle) = mock::pair::<(), ()>();
        let mut dispatch = task::spawn(run(inner, rx, semaphore, || idle(max_idle)));
        handle.allow(1);

        // Service ready without requests. Idle counter starts ticking.
        assert_pending!(dispatch.poll());

        sleep(max_idle).await;

        assert_ready!(dispatch.poll());
        drop((tx, handle));
    }

    #[tokio::test]
    async fn idle_reset_by_request() {
        let max_idle = Duration::from_millis(100);

        let semaphore = Arc::new(Semaphore::new(1));
        let (tx, rx) = mpsc::unbounded_channel();
        let (inner, mut handle) = mock::pair::<(), ()>();
        let mut dispatch = task::spawn(run(inner, rx, semaphore.clone(), || idle(max_idle)));
        handle.allow(1);

        // Service ready without requests. Idle counter starts ticking.
        assert_pending!(dispatch.poll());
        sleep(max_idle).await;

        // Send a request after the deadline has fired but before the
        // dispatch future is polled. Ensure that the request is admitted,
        // resetting idleness.
        let _permit = semaphore.acquire_owned().await;
        tx.send({
            let (tx, _rx) = oneshot::channel();
            super::InFlight {
                request: (),
                tx,
                _permit,
            }
        })
        .ok()
        .expect("request not sent");

        assert_pending!(dispatch.poll());

        handle.allow(1);
        assert_pending!(dispatch.poll());

        sleep(max_idle).await;

        assert_ready!(dispatch.poll());
        drop((tx, handle));
    }
}

#![recursion_limit = "256"]
use linkerd2_error::Error;
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

mod dispatch;
pub mod error;
mod layer;
mod service;

pub use self::{layer::SpawnBufferLayer, service::Buffer};

struct InFlight<Req, F> {
    request: Req,
    tx: tokio::sync::oneshot::Sender<Result<F, linkerd2_error::Error>>,
}

pub(crate) fn new<Req, S>(
    inner: S,
    capacity: usize,
    idle_timeout: Option<Duration>,
) -> (
    Buffer<Req, S::Future>,
    impl std::future::Future<Output = ()> + Send + 'static,
)
where
    Req: Send + 'static,
    S: tower::Service<Req> + Send + 'static,
    S::Error: Into<Error> + Send + 'static,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    use futures::future;

    let (tx, rx) = mpsc::channel(capacity);
    let (ready_tx, ready_rx) = watch::channel(Poll::Pending);
    let idle = move || match idle_timeout {
        Some(t) => future::Either::Left(dispatch::idle(t)),
        None => future::Either::Right(future::pending()),
    };
    let dispatch = dispatch::run(inner, rx, ready_tx, idle);
    (Buffer::new(tx, ready_rx), dispatch)
}

#[cfg(test)]
mod test {
    use std::task::Poll;
    use tokio_test::{assert_pending, assert_ready, assert_ready_ok, task};
    use tower_test::mock;

    #[test]
    fn propagates_readiness() {
        let (service, mut handle) = mock::pair::<(), ()>();
        let (service, dispatch) = super::new(service, 1, None);
        handle.allow(0);
        let mut service = mock::Spawn::new(service);
        let mut dispatch = task::spawn(dispatch);

        assert_pending!(dispatch.poll());
        assert_pending!(service.poll_ready());
        handle.allow(1);

        assert_pending!(dispatch.poll());
        assert_ready_ok!(service.poll_ready());

        // Consume the allowed call.
        drop(service.call(()));

        handle.send_error(Bad);
        assert_pending!(dispatch.poll());
        assert!(
            matches!(service.poll_ready(), Poll::Ready(Err(e)) if e.source().unwrap().is::<Bad>())
        );

        drop(service);
        assert_ready!(dispatch.poll());
    }

    #[derive(Debug)]
    struct Bad;
    impl std::error::Error for Bad {}
    impl std::fmt::Display for Bad {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "bad")
        }
    }
}

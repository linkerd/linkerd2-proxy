use linkerd2_error::Error;
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

mod dispatch;
pub mod error;
mod layer;
mod service;

pub use self::{dispatch::Dispatch, layer::SpawnBufferLayer, service::Buffer};

struct InFlight<Req, F> {
    request: Req,
    tx: tokio::sync::oneshot::Sender<Result<F, linkerd2_error::Error>>,
}

pub(crate) fn new<Req, S>(
    inner: S,
    capacity: usize,
    idle_timeout: Option<Duration>,
) -> (Buffer<Req, S::Future>, Dispatch<S, Req, S::Future>)
where
    Req: Send + 'static,
    S: tower::Service<Req> + Send + 'static,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    let (tx, rx) = mpsc::channel(capacity);
    let (ready_tx, ready_rx) = watch::channel(Poll::Pending);
    let dispatch = Dispatch::new(inner, rx, ready_tx, idle_timeout);
    (Buffer::new(tx, ready_rx), dispatch)
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::{future::Future, pin::Pin};
    use tokio_test::{assert_pending, assert_ready, assert_ready_ok, task};
    use tower::util::ServiceExt;
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
        assert_eq!(
            Poll::Ready(Err(String::from("bad"))),
            service.poll_ready().map_err(|e| e.to_string())
        );

        drop(service);
        assert_ready!(dispatch.poll());
    }

    #[tokio::test]
    async fn repolls_ready_on_notification() {
        struct ReadyNotify {
            notified: bool,
            _handle: Arc<()>,
        }
        impl tower::Service<()> for ReadyNotify {
            type Response = ();
            type Error = Bad;
            type Future = Pin<Box<dyn Future<Output = Result<(), Bad>> + Send + Sync + 'static>>;

            fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Bad>> {
                println!("Polling");
                if self.notified {
                    return Poll::Ready(Err(Bad));
                }

                println!("Notifying");
                cx.waker().wake_by_ref();
                self.notified = true;
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _: ()) -> Self::Future {
                unimplemented!("not called");
            }
        }

        let _handle = Arc::new(());
        let handle = Arc::downgrade(&_handle);
        let (mut service, dispatch) = super::new(
            ReadyNotify {
                _handle,
                notified: false,
            },
            1,
            None,
        );
        tokio::spawn(dispatch);
        let ret = service.ready_and().await;
        assert!(ret.is_err());
        assert!(
            handle.upgrade().is_none(),
            "inner service must be dropped on error"
        );
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

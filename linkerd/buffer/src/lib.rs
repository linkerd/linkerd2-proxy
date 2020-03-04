use futures::Async;
use linkerd2_error::Error;
use tokio::sync::{mpsc, watch};

mod dispatch;
pub mod error;
mod layer;
mod service;

pub use self::{dispatch::Dispatch, layer::SpawnBufferLayer, service::Buffer};

struct InFlight<Req, Rsp> {
    request: Req,
    tx: tokio::sync::oneshot::Sender<Result<Rsp, linkerd2_error::Error>>,
}

pub(crate) fn new<Req, S>(
    inner: S,
    capacity: usize,
) -> (Buffer<Req, S::Response>, Dispatch<S, Req, S::Response>)
where
    Req: Send + 'static,
    S: tower::Service<Req> + Send + 'static,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    let (tx, rx) = mpsc::channel(capacity);
    let (ready_tx, ready_rx) = watch::channel(Ok(Async::NotReady));
    let dispatch = Dispatch::new(inner, rx, ready_tx);
    (Buffer::new(tx, ready_rx), dispatch)
}

#[cfg(test)]
mod test {
    use futures::{future, Async, Future, Poll};
    use std::sync::Arc;
    use tower::util::ServiceExt;
    use tower::Service;
    use tower_test::mock;

    #[test]
    fn propagates_readiness() {
        run(|| {
            let (service, mut handle) = mock::pair::<(), ()>();
            let (mut service, mut dispatch) = super::new(service, 1);
            handle.allow(0);

            assert!(dispatch.poll().expect("never fails").is_not_ready());
            assert!(service.poll_ready().expect("must not fail").is_not_ready());
            handle.allow(1);

            assert!(dispatch.poll().expect("never fails").is_not_ready());
            assert!(service.poll_ready().expect("must not fail").is_ready());
            // Consume the allowed call.
            drop(service.call(()));

            handle.send_error(Bad);
            assert!(dispatch.poll().expect("never fails").is_not_ready());
            assert_eq!(
                "bad",
                service.poll_ready().expect_err("must fail").to_string()
            );

            drop(service);
            assert!(dispatch.poll().expect("never fails").is_ready());

            Ok::<(), ()>(())
        })
    }

    #[test]
    fn repolls_ready_on_notification() {
        struct ReadyNotify {
            notified: bool,
            _handle: Arc<()>,
        }
        impl tower::Service<()> for ReadyNotify {
            type Response = ();
            type Error = Bad;
            type Future = future::FutureResult<(), Bad>;

            fn poll_ready(&mut self) -> Poll<(), Bad> {
                println!("Polling");
                if self.notified {
                    return Err(Bad);
                }

                println!("Notifying");
                futures::task::current().notify();
                self.notified = true;
                Ok(Async::Ready(()))
            }

            fn call(&mut self, _: ()) -> Self::Future {
                unimplemented!("not called");
            }
        }

        run(|| {
            let _handle = Arc::new(());
            let handle = Arc::downgrade(&_handle);
            let (service, dispatch) = super::new(
                ReadyNotify {
                    _handle,
                    notified: false,
                },
                1,
            );
            tokio::spawn(dispatch.map_err(|_| ()));
            service.ready().then(move |ret| {
                assert!(ret.is_err());
                assert!(
                    handle.upgrade().is_none(),
                    "inner service must be dropped on error"
                );
                Ok::<(), ()>(())
            })
        })
    }

    #[derive(Debug)]
    struct Bad;
    impl std::error::Error for Bad {}
    impl std::fmt::Display for Bad {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "bad")
        }
    }

    fn run<F, R>(f: F)
    where
        F: FnOnce() -> R + 'static,
        R: future::IntoFuture<Item = ()> + 'static,
    {
        tokio::runtime::current_thread::run(future::lazy(f).map_err(|_| panic!("Failed")));
    }
}

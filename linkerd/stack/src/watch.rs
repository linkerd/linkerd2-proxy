use crate::{NewService, Param, Service};
use std::task::{Context, Poll};
use tokio::sync::watch;
use tracing::Instrument;

/// Builds [`SpawnWatch`] services, where an inner [`Service`] is updated
/// dynamically by a background task as a target wrapped in a
/// [`watch::Receiver`] changes.
///
/// `N` is a [`NewService`] which produces types implementing [`UpdateWatch`],
/// which define the logic for how to update the inner [`Service`].
#[derive(Clone, Debug)]
pub struct NewSpawnWatch<P, N> {
    inner: N,
    _marker: std::marker::PhantomData<fn(P)>,
}

/// A `S`-typed service which is updated dynamically by a background task.
///
/// Each clone of a `SpawnWatch` service that shares the same watch owns its own
/// clone of the inner service. As the `watch::Receiver` is updated, the
/// background task
#[derive(Clone, Debug)]
pub struct SpawnWatch<S> {
    rx: watch::Receiver<S>,
    inner: S,
}

// === impl NewSpawnWatch ===

impl<P, N> NewSpawnWatch<P, N> {
    pub fn new(inner: N) -> Self {
        Self {
            inner,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, P, N, S> NewService<T> for NewSpawnWatch<P, N>
where
    T: Param<watch::Receiver<P>>,
    P: Clone + Send + Sync + 'static,
    N: NewService<P, Service = S> + Clone + Send + 'static,
    S: Clone + Default + Send + Sync + 'static,
{
    type Service = SpawnWatch<S>;

    fn new_service(&self, target: T) -> Self::Service {
        let mut target_rx = target.param();

        let new_inner = self.inner.clone();
        let inner = new_inner.new_service((*target_rx.borrow_and_update()).clone());
        let (tx, rx) = watch::channel(inner.clone());

        tokio::spawn(
            async move {
                loop {
                    tokio::select! {
                        _ = tx.closed() => return,
                        res = target_rx.changed() => {
                            if res.is_err() {
                                return;
                            }
                        }
                    }

                    let tgt = (*target_rx.borrow_and_update()).clone();
                    if tx.send(new_inner.new_service(tgt)).is_err() {
                        return;
                    }
                }
            }
            .in_current_span(),
        );

        SpawnWatch { rx, inner }
    }
}

impl<Req, S> Service<Req> for SpawnWatch<S>
where
    S: Service<Req> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if matches!(self.rx.has_changed(), Ok(true)) {
            self.inner = self.rx.borrow_and_update().clone();
        }

        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewService;
    use tokio::sync::watch;
    use tower::ServiceExt;
    use tower_test::mock::{self, Mock};

    #[derive(Clone)]
    struct Update;

    /// Wrapper around `tower_test::mock::Mock` to implement `Default`.
    #[derive(Clone, Default)]
    struct DefaultMock(Option<Mock<(), ()>>);

    impl NewService<watch::Receiver<Mock<(), ()>>> for Update {
        type Service = Update;

        fn new_service(&self, _: watch::Receiver<Mock<(), ()>>) -> Self::Service {
            Update
        }
    }

    impl NewService<Mock<(), ()>> for Update {
        type Service = DefaultMock;

        fn new_service(&self, target: Mock<(), ()>) -> Self::Service {
            DefaultMock(Some(target))
        }
    }

    impl Service<()> for DefaultMock {
        type Response = ();
        type Error = <Mock<(), ()> as Service<()>>::Error;
        type Future = <Mock<(), ()> as Service<()>>::Future;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0
                .as_mut()
                .expect("default service isn't used")
                .poll_ready(cx)
        }

        fn call(&mut self, req: ()) -> Self::Future {
            self.0
                .as_mut()
                .expect("default service isn't used")
                .call(req)
        }
    }

    #[tokio::test]
    async fn switches_service() {
        let _trace = linkerd_tracing::test::trace_init();

        let (svc1, mut handle1) = mock::pair::<(), ()>();
        let (tx, rx) = watch::channel(svc1);

        let new_watch = NewSpawnWatch::new(Update);
        let mut watch_svc = new_watch.new_service(rx);

        tokio::spawn(async move {
            handle1.allow(1);
            handle1
                .next_request()
                .await
                .expect("should call inner service")
                .1
                .send_response(());
            handle1.send_error("old service shouldn't be called again");
        });

        // call the service once, using the initial service state
        watch_svc
            .ready()
            .await
            .unwrap()
            .call(())
            .await
            .expect("first request should succeed");
        tracing::info!("called first service");

        // update the service
        let (svc2, mut handle2) = mock::pair::<(), ()>();
        tx.send(svc2).expect("SpawnWatch task is alive");
        // ensure the background task runs
        tokio::task::yield_now().await;

        tokio::spawn(async move {
            handle2.allow(1);
            handle2
                .next_request()
                .await
                .expect("should call inner service")
                .1
                .send_response(());
        });

        // now, the second service should be used.
        watch_svc
            .ready()
            .await
            .unwrap()
            .call(())
            .await
            .expect("first request should succeed");
        tracing::info!("called second service");
    }
}

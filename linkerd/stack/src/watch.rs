use crate::{NewService, Param, Service};
use std::{
    marker::PhantomData,
    task::{Context, Poll},
};
use tokio::sync::watch;
use tracing::Instrument;

/// Builds [`SpawnWatch`] services from a target that produces a `P`-typed
/// [`watch::Receiver`].
///
/// A background task is spawned that updates the inner service when the watch
/// value changes.
#[derive(Debug)]
pub struct NewSpawnWatch<P, N> {
    inner: N,
    _marker: PhantomData<fn(P)>,
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

/// Builds `NewFromTuple` instances within a `NewSpawnWatch` stack.
///
/// When we use a `T`-typed target to produce a `U`-typed `watch::Receiver`,
/// this module converts the `U`-typed target value to a `P`-typed target for
/// the inner stack. `P` must implement `From<(U, T)>`.
#[derive(Debug)]
pub struct NewWatchedFromTuple<P, N> {
    inner: N,
    _marker: PhantomData<fn() -> P>,
}

/// Builds services by passing `P` typed values to an `N`-typed inner stack.
#[derive(Debug)]
pub struct NewFromTuple<T, P, N> {
    target: T,
    inner: N,
    _marker: PhantomData<fn() -> P>,
}

type SpawnFromTuple<P, T, N> = NewSpawnWatch<P, NewWatchedFromTuple<T, N>>;

// === impl NewSpawnWatch ===

impl<P, N> NewSpawnWatch<P, N> {
    pub fn new(inner: N) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    pub fn layer() -> impl tower::layer::Layer<N, Service = Self> + Clone {
        crate::layer::mk(Self::new)
    }

    /// Creates a new `NewSpawnWatch` layer that transforms the target type to
    /// `T` for the inner stack.
    pub fn layer_into<T>() -> impl tower::layer::Layer<N, Service = SpawnFromTuple<T, P, N>> + Clone
    {
        crate::layer::mk(|inner| NewSpawnWatch::new(NewWatchedFromTuple::new(inner)))
    }
}

impl<T, P, N, M, S> NewService<T> for NewSpawnWatch<P, N>
where
    T: Param<watch::Receiver<P>> + Clone + Send + 'static,
    P: Clone + Send + Sync + 'static,
    N: NewService<T, Service = M>,
    M: NewService<P, Service = S> + Send + 'static,
    S: Clone + Send + Sync + 'static,
{
    type Service = SpawnWatch<S>;

    fn new_service(&self, target: T) -> Self::Service {
        let mut target_rx = target.param();

        // Create a `NewService` that is used to process updates to the watched
        // value. This allows inner stacks to, for instance, scope caches to the
        // target.
        let new_inner = self.inner.new_service(target);

        let inner = {
            let p = (*target_rx.borrow_and_update()).clone();
            new_inner.new_service(p)
        };
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

                    let inner = {
                        let p = (*target_rx.borrow_and_update()).clone();
                        new_inner.new_service(p)
                    };
                    if tx.send(inner).is_err() {
                        return;
                    }
                }
            }
            .in_current_span(),
        );

        SpawnWatch { rx, inner }
    }
}

impl<P, N: Clone> Clone for NewSpawnWatch<P, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl SpawnWatch ===

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

// === impl NewWatchedFromTuple ===

impl<T, N> NewWatchedFromTuple<T, N> {
    fn new(inner: N) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }
}

impl<T, P, N, M> NewService<T> for NewWatchedFromTuple<P, N>
where
    T: Clone,
    N: NewService<T, Service = M>,
    M: NewService<P> + Send + 'static,
{
    type Service = NewFromTuple<T, P, M>;

    fn new_service(&self, target: T) -> Self::Service {
        // Create a `NewService` that is used to process updates to the watched
        // value. This allows inner stacks to, for instance, scope caches to the
        // target.
        let inner = self.inner.new_service(target.clone());

        NewFromTuple {
            target,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<P, N: Clone> Clone for NewWatchedFromTuple<P, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl NewFromTuple ===

impl<T, U, P, N> NewService<U> for NewFromTuple<T, P, N>
where
    T: Clone,
    P: From<(U, T)>,
    N: NewService<P>,
{
    type Service = N::Service;

    fn new_service(&self, target: U) -> Self::Service {
        let p = P::from((target, self.target.clone()));
        self.inner.new_service(p)
    }
}

impl<T: Clone, P, N: Clone> Clone for NewFromTuple<T, P, N> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewService;
    use tokio::sync::watch;
    use tower::ServiceExt;
    use tower_test::mock;

    #[derive(Clone)]
    struct Update;

    type Mock = mock::Mock<(), ()>;
    type MockRx = watch::Receiver<Mock>;

    /// Wrapper around `tower_test::mock::Mock` to implement `Default`.
    #[derive(Clone, Default)]
    struct DefaultMock(Option<Mock>);

    impl NewService<MockRx> for Update {
        type Service = Update;

        fn new_service(&self, _: MockRx) -> Self::Service {
            Update
        }
    }

    impl NewService<Mock> for Update {
        type Service = DefaultMock;

        fn new_service(&self, target: Mock) -> Self::Service {
            DefaultMock(Some(target))
        }
    }

    impl Service<()> for DefaultMock {
        type Response = ();
        type Error = <Mock as Service<()>>::Error;
        type Future = <Mock as Service<()>>::Future;

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

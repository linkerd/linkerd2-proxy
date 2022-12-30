use crate::{NewService, Service};
use parking_lot::Mutex;
use std::{
    sync::Arc,
    task::{Context, Poll},
};

/// Lazily constructs an inner stack when the service is first used.
#[derive(Clone, Debug)]
pub struct NewLazy<N>(N);

#[derive(Debug)]
pub struct Lazy<T, N, S> {
    inner: Option<S>,
    shared: Arc<Mutex<Shared<T, N, S>>>,
}

#[derive(Debug)]
enum Shared<T, N, S> {
    Uninit { inner: N, target: T },
    Service(S),
    Invalid,
}

// === impl NewLazy ===

impl<N> NewLazy<N> {
    pub fn layer() -> impl super::layer::Layer<N, Service = Self> + Clone {
        super::layer::mk(Self)
    }
}

impl<T, N> NewService<T> for NewLazy<N>
where
    N: NewService<T> + Clone,
{
    type Service = Lazy<T, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        Lazy {
            inner: None,
            shared: Arc::new(Mutex::new(Shared::Uninit {
                inner: self.0.clone(),
                target,
            })),
        }
    }
}

// === impl Lazy ===

impl<Req, T, N, S> Service<Req> for Lazy<T, N, S>
where
    N: NewService<T, Service = S>,
    S: Service<Req> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if let Some(inner) = self.inner.as_mut() {
            return inner.poll_ready(cx);
        }

        let mut shared = self.shared.lock();
        *shared = match std::mem::take(&mut *shared) {
            Shared::Uninit { inner, target } => {
                let svc = inner.new_service(target);
                self.inner = Some(svc.clone());
                Shared::Service(svc)
            }

            Shared::Service(svc) => {
                self.inner = Some(svc.clone());
                Shared::Service(svc)
            }

            Shared::Invalid => unreachable!(),
        };

        self.inner
            .as_mut()
            .expect("inner service must be set")
            .poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.as_mut().expect("polled").call(req)
    }
}

impl<T, N, S: Clone> Clone for Lazy<T, N, S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared: self.shared.clone(),
        }
    }
}

// === impl Shared ===

impl<T, N, S> Default for Shared<T, N, S> {
    fn default() -> Self {
        Shared::Invalid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::Layer;
    use std::sync::Arc;
    use tokio_test::{assert_pending, assert_ready_ok};
    use tower::ServiceExt;
    use tower_test::mock;

    #[derive(Clone)]
    struct NewOnce<T>(Arc<Mutex<Option<T>>>);

    impl<T> NewService<()> for NewOnce<T> {
        type Service = T;
        fn new_service(&self, _: ()) -> Self::Service {
            self.0
                .lock()
                .take()
                .expect("service should only be made once")
        }
    }

    fn new_once<T>(svc: T) -> NewOnce<T> {
        NewOnce(Arc::new(Mutex::new(Some(svc))))
    }

    #[tokio::test]
    async fn only_makes_service_once() {
        let new_svc = new_once(tower::service_fn(|_: ()| async { Ok::<_, ()>(()) }));
        let lazy = NewLazy::layer().layer(new_svc).new_service(());

        let t1 = tokio::spawn(lazy.clone().oneshot(()));
        let t2 = tokio::spawn(lazy.clone().oneshot(()));
        let t3 = tokio::spawn(lazy.clone().oneshot(()));

        t1.await.unwrap().unwrap();
        t2.await.unwrap().unwrap();
        t3.await.unwrap().unwrap();
    }

    /// Asserts that a `Lazy` service properly forwards the inner service's
    /// readiness after creating it.
    #[tokio::test]
    async fn preserves_inner_readiness() {
        let (svc, mut handle) = mock::pair::<(), ()>();
        let new_svc = new_once(svc);
        let lazy = NewLazy::layer().layer(new_svc).new_service(());
        let mut lazy = mock::Spawn::new(lazy);

        // the inner service is made, but isn't ready yet.
        handle.allow(0);
        assert_pending!(lazy.poll_ready());

        handle.allow(1);
        assert_ready_ok!(lazy.poll_ready());
    }

    #[tokio::test]
    async fn reusable() {
        let new_svc = new_once(tower::service_fn(|_: ()| async { Ok::<_, ()>(()) }));
        let mut lazy = NewLazy::layer().layer(new_svc).new_service(());

        lazy.ready()
            .await
            .expect("ready")
            .call(())
            .await
            .expect("call");

        lazy.ready()
            .await
            .expect("ready")
            .call(())
            .await
            .expect("call");

        lazy.ready()
            .await
            .expect("ready")
            .call(())
            .await
            .expect("call");
    }
}

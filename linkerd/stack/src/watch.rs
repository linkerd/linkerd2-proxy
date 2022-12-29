use crate::{NewService, Param, Service};
use std::task::{Context, Poll};
use tokio::sync::watch;
use tracing::Instrument;

pub trait UpdateWatch<T> {
    type Service: Clone + Default;

    fn update(&mut self, target: &T) -> Option<Self::Service>;
}

#[derive(Clone, Debug)]
pub struct NewSpawnWatch<P, N> {
    new_update: N,
    _marker: std::marker::PhantomData<fn(P)>,
}

#[derive(Clone, Debug)]
pub struct SpawnWatch<S> {
    rx: watch::Receiver<S>,
    inner: S,
}

impl<P, N> NewSpawnWatch<P, N> {
    pub fn new(new_update: N) -> Self {
        Self {
            new_update,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, P, N, U> NewService<T> for NewSpawnWatch<P, N>
where
    T: Param<watch::Receiver<P>> + Clone,
    P: Clone + Send + Sync + 'static,
    N: NewService<T, Service = U> + Send + 'static,
    U: UpdateWatch<P> + Send + Sync + 'static,
    U::Service: Send + Sync + 'static,
{
    type Service = SpawnWatch<U::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let mut target_rx = target.param();

        let mut update = self.new_update.new_service(target);
        let inner = update
            .update(&*target_rx.borrow_and_update())
            .unwrap_or_default();
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

                    if let Some(inner) = update.update(&*target_rx.borrow_and_update()) {
                        if tx.send(inner).is_err() {
                            return;
                        }
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

    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req)
    }
}

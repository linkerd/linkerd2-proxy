use crate::NewService;
use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;
use tower::util::{Oneshot, ServiceExt};

#[derive(Copy, Clone, Debug)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct NewPending<M> {
    inner: M,
}

/// Creates a `Service` immediately, even while the future making the service
/// is still pending.
pub enum Pending<F, S> {
    Making(F),
    Made(S),
}

pub fn layer() -> Layer {
    Layer(())
}

// === impl Layer ===

impl<M> tower::layer::Layer<M> for Layer {
    type Service = NewPending<M>;

    fn layer(&self, inner: M) -> Self::Service {
        NewPending { inner }
    }
}

// === impl NewPending ===

impl<T, M> NewService<T> for NewPending<M>
where
    M: tower::Service<T> + Clone,
{
    type Service = Pending<Oneshot<M, T>, <M as tower::Service<T>>::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        let fut = self.inner.clone().oneshot(target);
        Pending::Making(fut)
    }
}

// === impl Pending ===

impl<F, S, Req> tower::Service<Req> for Pending<F, S>
where
    F: Future<Item = S>,
    F::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = futures::future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            *self = match self {
                Pending::Making(fut) => {
                    let svc = try_ready!(fut.poll().map_err(Into::into));
                    Pending::Made(svc)
                }
                Pending::Made(svc) => return svc.poll_ready().map_err(Into::into),
            };
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        if let Pending::Made(ref mut s) = self {
            return s.call(req).map_err(Into::into);
        }

        panic!("pending not ready yet");
    }
}

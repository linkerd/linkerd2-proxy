use crate::svc::{self, ServiceExt};
use crate::Error;
use futures::{try_ready, Future, Poll};
use linkerd2_router as rt;

#[derive(Copy, Clone, Debug)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct MakePending<M> {
    inner: M,
}

/// Creates a `Service` immediately, even while the future making the service
/// is still pending.
pub enum Pending<F, S> {
    Making(F),
    Made(S),
}

pub type Svc<M, T> = Pending<svc::Oneshot<M, T>, <M as svc::Service<T>>::Response>;

pub fn layer() -> Layer {
    Layer(())
}

// === impl Layer ===

impl<M> svc::Layer<M> for Layer {
    type Service = MakePending<M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakePending { inner }
    }
}

// === impl MakePending ===

impl<T, M> rt::Make<T> for MakePending<M>
where
    M: svc::Service<T> + Clone,
    M::Error: Into<Error>,
    T: Clone,
{
    type Value = Svc<M, T>;

    fn make(&self, target: &T) -> Self::Value {
        let fut = self.inner.clone().oneshot(target.clone());
        Pending::Making(fut)
    }
}

// === impl Pending ===

impl<F, S, Req> svc::Service<Req> for Pending<F, S>
where
    F: Future<Item = S>,
    F::Error: Into<Error>,
    S: svc::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = futures::future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        let mut svc = match self {
            Pending::Making(fut) => try_ready!(fut.poll().map_err(Into::into)),
            Pending::Made(s) => return s.poll_ready().map_err(Into::into),
        };

        let ret = svc.poll_ready().map_err(Into::into);
        *self = Pending::Made(svc);
        ret
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self {
            Pending::Making(_) => panic!("pending not ready yet"),
            Pending::Made(s) => s.call(req).map_err(Into::into),
        }
    }
}

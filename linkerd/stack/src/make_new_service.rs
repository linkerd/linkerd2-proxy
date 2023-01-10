use crate::{NewService, Service};
use futures::TryFuture;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Debug, Clone)]
pub struct MakeNewService<N, S> {
    new_svc: N,
    target: S,
}

#[derive(Debug, Clone)]
#[pin_project::pin_project]
pub struct ResponseFuture<T, F, N> {
    #[pin]
    future: F,
    new_svc: N,
    target: Option<T>,
}

// === impl MakeNewService ===

impl<N: Clone, S> MakeNewService<N, S> {
    pub fn layer(new_svc: N) -> impl crate::layer::Layer<S, Service = Self> + Clone {
        crate::layer::mk(move |target| Self {
            target,
            new_svc: new_svc.clone(),
        })
    }
}

impl<N, S, T> Service<T> for MakeNewService<N, S>
where
    S: Service<T>,
    N: NewService<(S::Response, T)> + Clone,
    T: Clone,
{
    type Response = N::Service;
    type Error = S::Error;
    type Future = ResponseFuture<T, S::Future, N>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.target.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.target.call(target.clone());
        ResponseFuture {
            future,
            new_svc: self.new_svc.clone(),
            target: Some(target),
        }
    }
}

// === impl ResponseFuture ===

impl<T, F, N> Future for ResponseFuture<T, F, N>
where
    F: TryFuture,
    N: NewService<(F::Ok, T)>,
{
    type Output = Result<N::Service, F::Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let discovered = futures::ready!(this.future.try_poll(cx))?;
        let target = this.target.take().expect("polled after ready");
        let svc = this.new_svc.new_service((discovered, target));
        Poll::Ready(Ok(svc))
    }
}

use futures::TryFuture;
use linkerd2_proxy_core::resolve::{self, Resolution, Update};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub fn make_unpin<R>(r: R) -> Resolve<R> {
    Resolve(r)
}

#[derive(Clone)]
pub struct Resolve<T>(T);

#[pin_project]
pub struct MakeUnpin<T>(Pin<Box<T>>);

impl<T, S> tower::Service<T> for Resolve<S>
where
    S: resolve::Resolve<T>,
{
    type Response = MakeUnpin<S::Resolution>;
    type Error = S::Error;
    type Future = MakeUnpin<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        MakeUnpin::new(self.0.resolve(target))
    }
}

impl<T> MakeUnpin<T> {
    fn new(t: T) -> Self {
        Self(Box::pin(t))
    }
}

impl<T: Resolution> Resolution for MakeUnpin<T> {
    type Endpoint = T::Endpoint;
    type Error = T::Error;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Update<Self::Endpoint>, Self::Error>> {
        self.project().0.as_mut().as_mut().poll(cx)
    }
}

impl<T: TryFuture> Future for MakeUnpin<T> {
    type Output = Result<MakeUnpin<T::Ok>, T::Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = futures::ready!(Pin::as_mut(self.project().0).try_poll(cx))?;
        Poll::Ready(Ok(MakeUnpin::new(res)))
    }
}

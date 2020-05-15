//! A middleware that wraps `Resolutions`, modifying their endpoint type.

use futures::{ready, TryFuture};
use linkerd2_proxy_core::resolve;
use pin_project::pin_project;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

pub trait MapEndpoint<Target, In> {
    type Out;
    fn map_endpoint(&self, target: &Target, addr: SocketAddr, in_ep: In) -> Self::Out;
}

#[derive(Clone, Debug)]
pub struct Resolve<M, R> {
    resolve: R,
    map: M,
}

#[pin_project]
#[derive(Debug)]
pub struct ResolveFuture<T, F, M> {
    #[pin]
    future: F,
    target: Option<T>,
    map: Option<M>,
}

#[pin_project]
#[derive(Clone, Debug)]
pub struct Resolution<T, M, R> {
    #[pin]
    resolution: R,
    target: T,
    map: M,
}

// === impl Resolve ===

impl<M, R> Resolve<M, R> {
    pub fn new<T>(map: M, resolve: R) -> Self
    where
        Self: resolve::Resolve<T>,
    {
        Self { resolve, map }
    }
}

impl<T, M, R> tower::Service<T> for Resolve<M, R>
where
    T: Clone,
    R: resolve::Resolve<T>,
    M: MapEndpoint<T, R::Endpoint> + Clone,
{
    type Response = Resolution<T, M, R::Resolution>;
    type Error = R::Error;
    type Future = ResolveFuture<T, R::Future, M>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.resolve.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        let future = self.resolve.resolve(target.clone());
        Self::Future {
            future,
            target: Some(target),
            map: Some(self.map.clone()),
        }
    }
}

// === impl ResolveFuture ===

impl<T, F, M> Future for ResolveFuture<T, F, M>
where
    F: TryFuture,
    F::Ok: resolve::Resolution,
    M: MapEndpoint<T, <F::Ok as resolve::Resolution>::Endpoint>,
{
    type Output = Result<Resolution<T, M, F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let resolution = ready!(this.future.try_poll(cx))?;
        let target = this.target.take().expect("polled after ready");
        let map = this.map.take().expect("polled after ready");
        Poll::Ready(Ok(Resolution {
            resolution,
            target,
            map,
        }))
    }
}

// === impl Resolution ===

impl<T, M, R> resolve::Resolution for Resolution<T, M, R>
where
    R: resolve::Resolution,
    M: MapEndpoint<T, R::Endpoint>,
{
    type Endpoint = M::Out;
    type Error = R::Error;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<resolve::Update<M::Out>, Self::Error>> {
        let this = self.project();
        let update = match ready!(this.resolution.poll(cx))? {
            resolve::Update::Add(eps) => resolve::Update::Add(
                eps.into_iter()
                    .map(|(a, ep)| {
                        let ep = this.map.map_endpoint(&this.target, a, ep);
                        (a, ep)
                    })
                    .collect(),
            ),
            resolve::Update::Remove(addrs) => resolve::Update::Remove(addrs),
            resolve::Update::DoesNotExist => resolve::Update::DoesNotExist,
            resolve::Update::Empty => resolve::Update::Empty,
        };
        Poll::Ready(Ok(update))
    }
}

// === impl MapEndpoint ===

impl<T, N> MapEndpoint<T, N> for () {
    type Out = N;

    fn map_endpoint(&self, _: &T, _: SocketAddr, ep: N) -> Self::Out {
        ep
    }
}

impl<T, In, Out, F: Fn(&T, SocketAddr, In) -> Out> MapEndpoint<T, In> for F {
    type Out = Out;

    fn map_endpoint(&self, target: &T, addr: SocketAddr, ep: In) -> Self::Out {
        (self)(target, addr, ep)
    }
}

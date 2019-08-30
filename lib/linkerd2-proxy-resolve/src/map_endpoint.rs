//! A middleware that wraps `Resolutions`, modifying their endpoint type.

use futures::{try_ready, Async, Future, Poll};
use linkerd2_proxy_core::resolve;

pub trait MapEndpoint<In> {
    type Out;
    fn map_endpoint(&self, in_ep: In) -> Self::Out;
}

#[derive(Clone, Debug)]
pub struct Resolve<R, M> {
    resolve: R,
    map: M,
}

#[derive(Debug)]
pub struct ResolveFuture<F, M> {
    future: F,
    map: Option<M>,
}

#[derive(Clone, Debug)]
pub struct Resolution<D, M> {
    resolution: D,
    map: M,
}

// === impl Resolve ===

impl<R, M> Resolve<R, M> {
    pub fn new<T>(resolve: R, map: M) -> Self
    where
        Self: resolve::Resolve<T>,
    {
        Self { resolve, map }
    }
}

impl<T, R, M> tower::Service<T> for Resolve<R, M>
where
    R: resolve::Resolve<T>,
    M: MapEndpoint<R::Endpoint> + Clone,
{
    type Response = Resolution<R::Resolution, M>;
    type Error = R::Error;
    type Future = ResolveFuture<R::Future, M>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.resolve.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        Self::Future {
            future: self.resolve.resolve(target),
            map: Some(self.map.clone()),
        }
    }
}

// === impl ResolveFuture ===

impl<F, M> Future for ResolveFuture<F, M>
where
    F: Future,
    F::Item: resolve::Resolution,
    M: MapEndpoint<<F::Item as resolve::Resolution>::Endpoint>,
{
    type Item = Resolution<F::Item, M>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let resolution = try_ready!(self.future.poll());
        let map = self.map.take().expect("polled after ready");
        Ok(Async::Ready(Resolution { resolution, map }))
    }
}

// === impl Resolution ===

impl<R, M> resolve::Resolution for Resolution<R, M>
where
    R: resolve::Resolution,
    M: MapEndpoint<R::Endpoint>,
{
    type Endpoint = M::Out;
    type Error = R::Error;

    fn poll(&mut self) -> Poll<resolve::Update<M::Out>, Self::Error> {
        let update = match try_ready!(self.resolution.poll()) {
            resolve::Update::Add(eps) => resolve::Update::Add(
                eps.into_iter()
                    .map(|(a, ep)| (a, self.map.map_endpoint(ep)))
                    .collect(),
            ),
            resolve::Update::Remove(addrs) => resolve::Update::Remove(addrs),
            resolve::Update::DoesNotExist => resolve::Update::DoesNotExist,
            resolve::Update::Empty => resolve::Update::Empty,
        };
        Ok(update.into())
    }
}

// === impl MapEndpoint ===

impl<T> MapEndpoint<T> for () {
    type Out = T;

    fn map_endpoint(&self, ep: T) -> Self::Out {
        ep
    }
}

impl<In, Out, F: Fn(In) -> Out> MapEndpoint<In> for F {
    type Out = Out;

    fn map_endpoint(&self, ep: In) -> Self::Out {
        (self)(ep)
    }
}

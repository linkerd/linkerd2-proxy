use super::discover::Discover;
use futures::{try_ready, Async, Future, Poll};
use linkerd2_proxy_core::resolve::{Resolution, Resolve};
use std::fmt;

#[derive(Clone, Debug)]
pub struct Layer<R> {
    resolve: R,
}

#[derive(Clone, Debug)]
pub struct MakeDiscover<R, M> {
    resolve: R,
    make_endpoint: M,
}

#[derive(Debug)]
pub struct DiscoverFuture<F, M> {
    future: F,
    make_endpoint: Option<M>,
}

// === impl Layer ===

pub fn layer<T, R>(resolve: R) -> Layer<R>
where
    R: Resolve<T> + Clone,
    R::Endpoint: fmt::Debug,
{
    Layer { resolve }
}

impl<R, M> tower::layer::Layer<M> for Layer<R>
where
    R: Clone,
{
    type Service = MakeDiscover<R, M>;

    fn layer(&self, make_endpoint: M) -> Self::Service {
        MakeDiscover {
            resolve: self.resolve.clone(),
            make_endpoint,
        }
    }
}

// === impl MakeDiscover ===

impl<T, R, M> tower::Service<T> for MakeDiscover<R, M>
where
    R: Resolve<T> + Clone,
    R::Endpoint: fmt::Debug + Clone + PartialEq,
    M: tower::Service<R::Endpoint> + Clone,
{
    type Response = Discover<R::Resolution, M>;
    type Error = <R::Future as Future>::Error;
    type Future = DiscoverFuture<R::Future, M>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.resolve.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.resolve.resolve(target);
        DiscoverFuture {
            future,
            make_endpoint: Some(self.make_endpoint.clone()),
        }
    }
}

// === impl DiscoverFuture ===

impl<F, M> Future for DiscoverFuture<F, M>
where
    F: Future,
    F::Item: Resolution,
    <F::Item as Resolution>::Endpoint: fmt::Debug + Clone + PartialEq,
    M: tower::Service<<F::Item as Resolution>::Endpoint>,
{
    type Item = Discover<F::Item, M>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let resolution = try_ready!(self.future.poll());
        let make_endpoint = self.make_endpoint.take().expect("polled after ready");
        Ok(Async::Ready(Discover::new(resolution, make_endpoint)))
    }
}

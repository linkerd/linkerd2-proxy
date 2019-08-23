use super::discover::Discover;
use futures::{try_ready, Async, Future, Poll};
use linkerd2_proxy_core::resolve::Resolve;
use std::fmt;

#[derive(Clone, Debug)]
pub struct Layer<R> {
    resolve: R,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<R, M> {
    resolve: R,
    make_endpoint: M,
}

pub struct DiscoverFuture<T, R: Resolve<T>, M: tower::Service<R::Endpoint>> {
    future: R::Future,
    discover: Option<Discover<T, R, M>>,
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
    type Service = MakeSvc<R, M>;

    fn layer(&self, make_endpoint: M) -> Self::Service {
        MakeSvc {
            resolve: self.resolve.clone(),
            make_endpoint,
        }
    }
}

// === impl MakeSvc ===

impl<T, R, M> tower::Service<T> for MakeSvc<R, M>
where
    T: Clone,
    R: Resolve<T> + Clone,
    R::Endpoint: fmt::Debug,
    M: tower::Service<R::Endpoint> + Clone,
{
    type Response = Discover<T, R, M>;
    type Error = <R::Future as Future>::Error;
    type Future = DiscoverFuture<T, R, M>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.resolve.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let discover = Discover::new(
            target.clone(),
            self.resolve.clone(),
            self.make_endpoint.clone(),
        );
        let future = self.resolve.resolve(target);
        DiscoverFuture {
            future,
            discover: Some(discover),
        }
    }
}

// === impl DiscoverFuture ===

impl<T, R, M> Future for DiscoverFuture<T, R, M>
where
    R: Resolve<T>,
    M: tower::Service<R::Endpoint>,
{
    type Item = Discover<T, R, M>;
    type Error = R::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let resolution = try_ready!(self.future.poll());
        let discover = self.discover.take().expect("polled after ready");
        Ok(Async::Ready(discover.with_resolution(resolution)))
    }
}

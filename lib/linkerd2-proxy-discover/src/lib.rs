#![deny(warnings, rust_2018_idioms)]

use linkerd2_discover_buffer::Buffer;
use linkerd2_proxy_core::Resolve;
use std::fmt;

pub mod from_resolve;

use self::from_resolve::FromResolve;

#[derive(Clone, Debug)]
pub struct Layer<T, R> {
    capacity: usize,
    resolve: R,
    _marker: std::marker::PhantomData<fn(T)>,
}

// === impl Layer ===

impl<T, R> Layer<T, R> {
    pub fn new(capacity: usize, resolve: R) -> Self {
        Self {
            capacity,
            resolve,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, R, M> tower::layer::Layer<M> for Layer<T, R>
where
    T: fmt::Display + Send + Clone,
    R: Resolve<T> + Send + Clone,
    R::Endpoint: fmt::Debug + Clone + PartialEq,
    R::Resolution: Send,
    M: tower::Service<R::Endpoint> + Clone + Send,
    FromResolve<R, M>: tower::Service<T>,
    <FromResolve<R, M> as tower::Service<T>>::Response: tower::discover::Discover + Send + 'static,
    <<FromResolve<R, M> as tower::Service<T>>::Response as tower::discover::Discover>::Key: Send,
    <<FromResolve<R, M> as tower::Service<T>>::Response as tower::discover::Discover>::Service:
        Send,
    <<FromResolve<R, M> as tower::Service<T>>::Response as tower::discover::Discover>::Error:
        std::error::Error,
    Buffer<FromResolve<R, M>>: tower::Service<T>,
    <Buffer<FromResolve<R, M>> as tower::Service<T>>::Response: tower::discover::Discover,
{
    type Service = Buffer<FromResolve<R, M>>;

    fn layer(&self, make_endpoint: M) -> Self::Service {
        let make_discover = FromResolve::new(self.resolve.clone(), make_endpoint);
        Buffer::new(self.capacity, make_discover)
    }
}

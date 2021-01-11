//! A `Service` middleware that applies arbitrary-user provided logic to each
//! target before it is issued to an inner service.

use linkerd2_error::Error;
pub use tower::filter::{Filter, FilterLayer, Predicate};

#[derive(Clone, Debug)]
pub struct FilterNewService<I, S> {
    filter: I,
    service: S,
}

// === impl RequestFilter ===

impl<I, S> FilterNewService<I, S> {
    pub fn new(filter: I, service: S) -> Self {
        Self { filter, service }
    }

    pub fn layer(filter: I) -> impl tower::Layer<S> + Clone
    where
        I: Clone,
    {
        crate::layer::mk(move |service| Self::new(filter.clone(), service))
    }
}

impl<T, F, S> super::NewService<T> for FilterNewService<F, S>
where
    F: Predicate<T>,
    S: super::NewService<F::Request>,
{
    type Service = super::ResultService<S::Service, Error>;

    fn new_service(&mut self, request: T) -> Self::Service {
        self.filter
            .check(request)
            .map(move |req| super::ResultService::ok(self.service.new_service(req)))
            .unwrap_or_else(super::ResultService::err)
    }
}

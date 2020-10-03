//! A `Service` middleware that applies arbitrary-user provided logic to each
//! target before it is issued to an inner service.

#![deny(warnings, rust_2018_idioms)]

use futures::future;
use linkerd2_stack::layer;
use std::task::{Context, Poll};

pub trait FilterRequest<Req> {
    type Request;
    type Error;

    fn filter(&self, request: Req) -> Result<Self::Request, Self::Error>;
}

#[derive(Clone, Debug)]
pub struct RequestFilter<I, S> {
    filter: I,
    service: S,
}

// === impl RequestFilter ===

impl<I, S> RequestFilter<I, S> {
    pub fn new(filter: I, service: S) -> Self {
        Self { filter, service }
    }

    pub fn layer(filter: I) -> impl layer::Layer<S, Service = Self> + Clone
    where
        I: Clone,
    {
        layer::mk(move |inner| Self::new(filter.clone(), inner))
    }
}

impl<T, F, S> tower::Service<T> for RequestFilter<F, S>
where
    F: FilterRequest<T>,
    S: tower::Service<F::Request, Error = F::Error>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = future::Either<S::Future, future::Ready<Result<S::Response, F::Error>>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: T) -> Self::Future {
        match self.filter.filter(request) {
            Ok(req) => {
                tracing::trace!("accepted");
                future::Either::Left(self.service.call(req))
            }
            Err(e) => {
                tracing::trace!("rejected");
                future::Either::Right(future::err(e))
            }
        }
    }
}

//! A `Service` middleware that applies arbitrary-user provided logic to each
//! target before it is issued to an inner service.

use futures::{future, prelude::*};
use linkerd2_error::Error;
use std::task::{Context, Poll};

pub trait FilterRequest<Req> {
    type Request;

    fn filter(&self, request: Req) -> Result<Self::Request, Error>;
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
}

impl<T, F, S> super::NewService<T> for RequestFilter<F, S>
where
    F: FilterRequest<T>,
    S: super::NewService<F::Request>,
{
    type Service = super::ResultService<S::Service, Error>;

    fn new_service(&mut self, request: T) -> Self::Service {
        self.filter
            .filter(request)
            .map(move |req| super::ResultService::ok(self.service.new_service(req)))
            .unwrap_or_else(super::ResultService::err)
    }
}

impl<T, F, S> tower::Service<T> for RequestFilter<F, S>
where
    F: FilterRequest<T>,
    S: tower::Service<F::Request>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::ErrInto<S::Future, Error>,
        future::Ready<Result<S::Response, Error>>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: T) -> Self::Future {
        self.filter
            .filter(request)
            .map(move |req| future::Either::Left(self.service.call(req).err_into::<Error>()))
            .unwrap_or_else(|e| future::Either::Right(future::err(e)))
    }
}

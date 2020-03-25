//! A `Service` middleware that applies arbitrary-user provided logic to each
//! target before it is issued to an inner service.

#![deny(warnings, rust_2018_idioms)]

use futures::{Future, Poll};

pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

pub trait RequestFilter<T> {
    type Error: Into<Error>;

    fn filter(&self, request: T) -> Result<T, Self::Error>;
}

#[derive(Clone, Debug)]
pub struct Service<I, S> {
    filter: I,
    service: S,
}

#[derive(Debug)]
pub enum ResponseFuture<F> {
    Future(F),
    Rejected(Option<Error>),
}

// === impl Service ===

impl<I, S> Service<I, S> {
    pub fn new(filter: I, service: S) -> Self {
        Self { filter, service }
    }
}

impl<T, I, S> tower::Service<T> for Service<I, S>
where
    I: RequestFilter<T>,
    S: tower::Service<T>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future>;

    #[inline]
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, request: T) -> Self::Future {
        match self.filter.filter(request) {
            Ok(req) => {
                tracing::trace!("accepted");
                let f = self.service.call(req);
                ResponseFuture::Future(f)
            }
            Err(e) => {
                tracing::trace!("rejected");
                ResponseFuture::Rejected(Some(e.into()))
            }
        }
    }
}

// === impl ResponseFuture ===

impl<F> Future for ResponseFuture<F>
where
    F: Future,
    F::Error: Into<Error>,
{
    type Item = F::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            ResponseFuture::Future(ref mut f) => f.poll().map_err(Into::into),
            ResponseFuture::Rejected(ref mut e) => Err(e.take().unwrap()),
        }
    }
}

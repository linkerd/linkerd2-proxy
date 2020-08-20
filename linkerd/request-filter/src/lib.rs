//! A `Service` middleware that applies arbitrary-user provided logic to each
//! target before it is issued to an inner service.

#![deny(warnings, rust_2018_idioms)]

use linkerd2_error::Error;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub trait RequestFilter<T> {
    type Error: Into<Error>;

    fn filter(&self, request: T) -> Result<T, Self::Error>;
}

#[derive(Clone, Debug)]
pub struct RequestFilterLayer<T> {
    filter: T,
}

#[derive(Clone, Debug)]
pub struct Service<I, S> {
    filter: I,
    service: S,
}

#[pin_project(project = ResponseFutureProj)]
#[derive(Debug)]
pub enum ResponseFuture<F> {
    Future(#[pin] F),
    Rejected(Option<Error>),
}

// === impl Layer ===

impl<T: Clone> RequestFilterLayer<T> {
    pub fn new(filter: T) -> Self {
        Self { filter }
    }
}

impl<T: Clone, S> tower::Layer<S> for RequestFilterLayer<T> {
    type Service = Service<T, S>;

    fn layer(&self, inner: S) -> Self::Service {
        Service::new(self.filter.clone(), inner)
    }
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
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(Into::into)
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

impl<F, T, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<T, E>>,
    E: Into<Error>,
{
    type Output = Result<T, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ResponseFutureProj::Future(f) => f.poll(cx).map(|r| r.map_err(Into::into)),
            ResponseFutureProj::Rejected(e) => Poll::Ready(Err(e.take().unwrap())),
        }
    }
}

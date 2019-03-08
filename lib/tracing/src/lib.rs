extern crate futures;
extern crate linkerd2_stack;
extern crate tokio_connect;
extern crate tower_service as svc;

use futures::{Future, Poll, Async};
use std::{error, fmt};
use tokio_connect::Connect;

pub mod stack;
mod span;

/// A Tracing construct that wraps an underlying operation.
#[derive(Debug, Clone)]
pub struct Tracing<T> {
    inner: T,
    name: String,
    span: Option<span::Span>
}

/// An error representing a Tracing error.
#[derive(Debug)]
pub enum Error<E> {
    /// Indicates that the underlying operation failed.
    Error(E),
}

//===== impl Tracing =====

impl<T> Tracing<T> {
    /// Construct a new `Tracing` wrapping `inner`.
    pub fn new(inner: T, name: String, span: Option<span::Span>) -> Self {
        Tracing { inner, name, span: span}
    }
}

impl<S, T, E, Req> svc::Service<Req> for Tracing<S>
where
    S: svc::Service<Req, Response = T, Error = E>,
{
    type Response = T;
    type Error = Error<E>;
    type Future = Tracing<S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(|e| Error::Error(e))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let inner = self.inner.call(req);
        let span: Option<span::Span> = span::SpanContext::new().span_from_request(String::from("spanName"), String::from("request_place_holder"));
        Tracing {
            inner,
            name: self.name.clone(),
            span: span
        }
    }
}

impl<C> Connect for Tracing<C>
where
    C: Connect,
{
    type Connected = C::Connected;
    type Error = Error<C::Error>;
    type Future = Tracing<C::Future>;

    fn connect(&self) -> Self::Future {
        let inner = self.inner.connect();
        let span: Option<span::Span> = span::SpanContext::new().span_from_request(String::from("spanName"), String::from("request_place_holder"));
        Tracing {
            inner,
            name: self.name.clone(),
            span: span
        }
    }
}

impl<F> Future for Tracing<F>
where
    F: Future,
{
    type Item = F::Item;
    type Error = Error<F::Error>;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll().map(|value| {
            match value {
                // close span on Ready
                Async::Ready(_) => {
                    if let Some(ref mut sampled_span) = self.span {
                        sampled_span.finish_span();
                    }
                },
                Async::NotReady => {

                }
            }
            value
        }).map_err(|e| {
            // close span on error
            if let Some(ref mut sampled_span) = self.span {
                sampled_span.finish_span();
            }
            Error::Error(e)
        })
    }
}

//===== impl Error =====

impl<E> fmt::Display for Error<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::Error(ref err) => fmt::Display::fmt(err, f),
        }
    }
}

impl<E> error::Error for Error<E>
where
    E: error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match *self {
            Error::Error(ref err) => Some(err),
        }
    }

    fn description(&self) -> &str {
        match *self {
            Error::Error(ref err) => err.description(),
        }
    }
}



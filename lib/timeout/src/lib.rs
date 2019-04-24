#[macro_use]
extern crate futures;
extern crate linkerd2_stack;
extern crate tokio_connect;
extern crate tokio_timer;
extern crate tower_service as svc;

use futures::{Future, Poll};
use std::time::Duration;
use tokio_connect::Connect;
use tokio_timer as timer;

pub mod error;
pub mod stack;

use self::error::{Error, Timedout};

/// A timeout that wraps an underlying operation.
#[derive(Debug, Clone)]
pub struct Timeout<T> {
    inner: T,
    duration: Duration,
}

//===== impl Timeout =====

impl<T> Timeout<T> {
    /// Construct a new `Timeout` wrapping `inner`.
    pub fn new(inner: T, duration: Duration) -> Self {
        Timeout { inner, duration }
    }

    fn timeout_error<E>(&self, error: timer::timeout::Error<E>) -> Error
    where
        E: Into<Error>,
    {
        match error {
            _ if error.is_timer() => error
                .into_timer()
                .expect("error.into_timer() must succeed if error.is_timer()")
                .into(),
            _ if error.is_elapsed() => Timedout(self.duration).into(),
            _ => error
                .into_inner()
                .expect("if error is not elapsed or timer, must be inner")
                .into(),
        }
    }
}

impl<S, T, Req> svc::Service<Req> for Timeout<S>
where
    S: svc::Service<Req, Response = T>,
    S::Error: Into<Error>,
{
    type Response = T;
    type Error = Error;
    type Future = Timeout<timer::Timeout<S::Future>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let inner = timer::Timeout::new(self.inner.call(req), self.duration);
        Timeout {
            inner,
            duration: self.duration,
        }
    }
}

impl<C> Connect for Timeout<C>
where
    C: Connect,
    C::Error: Into<Error>,
{
    type Connected = C::Connected;
    type Error = Error;
    type Future = Timeout<timer::Timeout<C::Future>>;

    fn connect(&self) -> Self::Future {
        let inner = timer::Timeout::new(self.inner.connect(), self.duration);
        Timeout {
            inner,
            duration: self.duration,
        }
    }
}

impl<F> Future for Timeout<timer::Timeout<F>>
where
    F: Future,
    F::Error: Into<Error>,
{
    type Item = F::Item;
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll().map_err(|e| self.timeout_error(e))
    }
}

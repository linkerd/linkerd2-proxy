extern crate futures;
extern crate linkerd2_stack;
extern crate tokio_connect;
extern crate tokio_timer;
extern crate tower_service as svc;

use futures::{Future, Poll};
use std::time::Duration;
use std::{error, fmt};
use tokio_connect::Connect;
use tokio_timer as timer;

pub mod stack;

/// A timeout that wraps an underlying operation.
#[derive(Debug, Clone)]
pub struct Timeout<T> {
    inner: T,
    duration: Duration,
}

/// An error representing that an operation timed out.
#[derive(Debug)]
pub enum Error<E> {
    /// Indicates the underlying operation timed out.
    Timeout(Duration),
    /// Indicates that the underlying operation failed.
    Error(E),
    // Indicates that the timer returned an error.
    Timer(timer::Error),
}

/// A duration which pretty-prints as fractional seconds.
#[derive(Copy, Clone, Debug)]
struct HumanDuration(pub Duration);

//===== impl Timeout =====

impl<T> Timeout<T> {
    /// Construct a new `Timeout` wrapping `inner`.
    pub fn new(inner: T, duration: Duration) -> Self {
        Timeout { inner, duration }
    }

    fn error<E>(&self, error: E) -> Error<E> {
        Error::Error(error)
    }

    fn timeout_error<E>(&self, error: timer::timeout::Error<E>) -> Error<E> {
        match error {
            _ if error.is_timer() => Error::Timer(
                error
                    .into_timer()
                    .expect("error.into_timer() must succeed if error.is_timer()"),
            ),
            _ if error.is_elapsed() => Error::Timeout(self.duration),
            _ => Error::Error(
                error
                    .into_inner()
                    .expect("if error is not elapsed or timer, must be inner"),
            ),
        }
    }
}

impl<S, T, E, Req> svc::Service<Req> for Timeout<S>
where
    S: svc::Service<Req, Response = T, Error = E>,
{
    type Response = T;
    type Error = Error<E>;
    type Future = Timeout<timer::Timeout<S::Future>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(|e| self.error(e))
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
{
    type Connected = C::Connected;
    type Error = Error<C::Error>;
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
    // F::Error: Error,
{
    type Item = F::Item;
    type Error = Error<F::Error>;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll().map_err(|e| self.timeout_error(e))
    }
}

//===== impl Error =====

impl<E> fmt::Display for Error<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::Timeout(ref d) => write!(f, "operation timed out after {}", HumanDuration(*d)),
            Error::Timer(ref err) => write!(f, "timer failed: {}", err),
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
            Error::Timer(ref err) => Some(err),
            _ => None,
        }
    }

    fn description(&self) -> &str {
        match *self {
            Error::Timeout(_) => "operation timed out",
            Error::Error(ref err) => err.description(),
            Error::Timer(ref err) => err.description(),
        }
    }
}

//===== impl HumanDuration =====

impl fmt::Display for HumanDuration {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let secs = self.0.as_secs();
        let subsec_ms = self.0.subsec_nanos() as f64 / 1_000_000f64;
        if secs == 0 {
            write!(fmt, "{}ms", subsec_ms)
        } else {
            write!(fmt, "{}s", secs as f64 + subsec_ms)
        }
    }
}

impl From<Duration> for HumanDuration {
    #[inline]
    fn from(d: Duration) -> Self {
        HumanDuration(d)
    }
}

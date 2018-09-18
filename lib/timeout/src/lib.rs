extern crate futures;
extern crate tokio_connect;
extern crate tokio_timer;
extern crate tower_service;

use futures::{Future, Poll};
use tokio_connect::Connect;
use tokio_timer::{self as timer, clock, Deadline, DeadlineError};
use tower_service::Service;
use std::{error, fmt};
use std::time::Duration;

/// A timeout that wraps an underlying operation.
#[derive(Debug, Clone)]
pub struct Timeout<T> {
    inner: T,
    duration: Duration,
}


/// An error representing that an operation timed out.
#[derive(Debug)]
pub struct Error<E> {
    kind: ErrorKind<E>,
}

#[derive(Debug)]
enum ErrorKind<E> {
    /// Indicates the underlying operation timed out.
    Timeout (Duration),
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
        Timeout {
            inner,
            duration,
        }
    }

    fn error<E>(&self, error: E) -> Error<E> {
        Error {
            kind: ErrorKind::Error(error),
        }
    }

    fn deadline_error<E>(&self, error: DeadlineError<E>) -> Error<E> {
        let kind = match error {
            _ if error.is_timer() =>
                ErrorKind::Timer(error.into_timer()
                    .expect("error.into_timer() must succeed if error.is_timer()")),
            _ if error.is_elapsed() =>
                ErrorKind::Timeout(self.duration),
            _ => ErrorKind::Error(error.into_inner()
                .expect("if error is not elapsed or timer, must be inner")),
        };
        Error { kind }
    }
}

impl<S, T, E> Service for Timeout<S>
where
    S: Service<Response=T, Error=E>,
{
    type Request = S::Request;
    type Response = T;
    type Error = Error<E>;
    type Future = Timeout<Deadline<S::Future>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(|e| self.error(e))
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let duration = self.duration;
        let deadline = clock::now() + duration;
        let inner = Deadline::new(self.inner.call(req), deadline);
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
    type Future = Timeout<Deadline<C::Future>>;

    fn connect(&self) -> Self::Future {
        let deadline = clock::now() + self.duration;
        let inner = Deadline::new(self.inner.connect(), deadline);
        Timeout {
            inner,
            duration: self.duration,
        }
    }
}

impl<F> Future for Timeout<Deadline<F>>
where
    F: Future,
    // F::Error: Error,
{
    type Item = F::Item;
    type Error = Error<F::Error>;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll().map_err(|e| self.deadline_error(e))
    }
}

//===== impl Error =====

impl<E> fmt::Display for Error<E>
where
    E: fmt::Display
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.kind {
            ErrorKind::Timeout(ref d) =>
                write!(f, "operation timed out after {}", HumanDuration(*d)),
            ErrorKind::Timer(ref err) => write!(f, "timer failed: {}", err),
            ErrorKind::Error(ref err) => fmt::Display::fmt(err, f),
        }
    }
}

impl<E> error::Error for Error<E>
where
    E: error::Error
{
    fn cause(&self) -> Option<&error::Error> {
        match self.kind {
            ErrorKind::Error(ref err) => Some(err),
            ErrorKind::Timer(ref err) => Some(err),
            _ => None,
        }
    }

    fn description(&self) -> &str {
        match self.kind {
            ErrorKind::Timeout(_) => "operation timed out",
            ErrorKind::Error(ref err) => err.description(),
            ErrorKind::Timer(ref err) => err.description(),
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

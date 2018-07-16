use futures::{Async, Future, Poll};
use http;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};
use tokio::timer::Delay;
use tower_service::Service;
use tower_h2;
use tower_reconnect::{Error as ReconnectError};

use timeout::TimeoutError;

// ===== Backoff =====

/// Wait a duration if inner `poll_ready` returns an error.
//TODO: move to tower-backoff
pub(super) struct Backoff<S> {
    inner: S,
    timer: Delay,
    waiting: bool,
    wait_dur: Duration,
}

impl<S> Backoff<S>
where
    S: Service,
{
    pub(super) fn new(inner: S, wait_dur: Duration) -> Self {
        Backoff {
            inner,
            timer: Delay::new(Instant::now() + wait_dur),
            waiting: false,
            wait_dur,
        }
    }

    fn poll_timer(&mut self) -> Poll<(), S::Error> {
        debug_assert!(self.waiting, "poll_timer expects to be waiting");

        match self.timer.poll().expect("timer shouldn't error") {
            Async::Ready(()) => {
                self.waiting = false;
                Ok(Async::Ready(()))
            }
            Async::NotReady => {
                Ok(Async::NotReady)
            }
        }
    }
}

impl<S> Service for Backoff<S>
where
    S: Service,
    S::Error: fmt::Debug,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if self.waiting {
            try_ready!(self.poll_timer());
        }

        match self.inner.poll_ready() {
            Err(_err) => {
                trace!("backoff: controller error, waiting {:?}", self.wait_dur);
                self.waiting = true;
                self.timer.reset(Instant::now() + self.wait_dur);
                self.poll_timer()
            }
            ok => ok,
        }
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.inner.call(req)
    }
}

/// Wraps an HTTP service, injecting authority and scheme on every request.
pub(super) struct AddOrigin<S> {
    authority: http::uri::Authority,
    inner: S,
    scheme: http::uri::Scheme,
}

impl<S> AddOrigin<S> {
    pub(super) fn new(scheme: http::uri::Scheme, auth: http::uri::Authority, service: S) -> Self {
        AddOrigin {
            authority: auth,
            inner: service,
            scheme,
        }
    }
}

impl<S, B> Service for AddOrigin<S>
where
    S: Service<Request = http::Request<B>>,
{
    type Request = http::Request<B>;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let (mut head, body) = req.into_parts();
        let mut uri: http::uri::Parts = head.uri.into();
        uri.scheme = Some(self.scheme.clone());
        uri.authority = Some(self.authority.clone());
        head.uri = http::Uri::from_parts(uri).expect("valid uri");

        self.inner.call(http::Request::from_parts(head, body))
    }
}

// ===== impl LogErrors

/// Log errors talking to the controller in human format.
pub(super) struct LogErrors<S> {
    inner: S,
}

// We want some friendly logs, but the stack of services don't have fmt::Display
// errors, so we have to build that ourselves. For now, this hard codes the
// expected error stack, and so any new middleware added will need to adjust this.
//
// The dead_code allowance is because rustc is being stupid and doesn't see it
// is used down below.
#[allow(dead_code)]
type LogError = ReconnectError<
    tower_h2::client::Error,
    tower_h2::client::ConnectError<
        TimeoutError<
            io::Error
        >
    >
>;

impl<S> LogErrors<S>
where
    S: Service<Error=LogError>,
{
    pub(super) fn new(service: S) -> Self {
        LogErrors {
            inner: service,
        }
    }
}

impl<S> Service for LogErrors<S>
where
    S: Service<Error=LogError>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(|e| {
            error!("controller error: {}", HumanError(&e));
            e
        })
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.inner.call(req)
    }
}

pub(super) struct HumanError<'a>(&'a LogError);

impl<'a> fmt::Display for HumanError<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self.0 {
            ReconnectError::Inner(ref e) => {
                fmt::Display::fmt(e, f)
            },
            ReconnectError::Connect(ref e) => {
                fmt::Display::fmt(e, f)
            },
            ReconnectError::NotReady => {
                // this error should only happen if we `call` the service
                // when it isn't ready, which is really more of a bug on
                // our side...
                f.pad("bug: called service when not ready")
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use tokio::runtime::current_thread::Runtime;

    struct MockService {
        polls: usize,
        succeed_after: usize,
    }

    impl Service for MockService {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = future::FutureResult<(), ()>;

        fn poll_ready(&mut self) -> Poll<(), ()> {
            self.polls += 1;
            if self.polls > self.succeed_after {
                Ok(().into())
            } else {
                Err(())
            }
        }

        fn call(&mut self, _req: ()) -> Self::Future {
            if self.polls > self.succeed_after {
                future::ok(())
            } else {
                future::err(())
            }
        }
    }

    fn succeed_after(cnt: usize) -> MockService {
        MockService {
            polls: 0,
            succeed_after: cnt,
        }
    }


    #[test]
    fn backoff() {
        let mock = succeed_after(2);
        let mut backoff = Backoff::new(mock, Duration::from_millis(5));
        let mut rt = Runtime::new().unwrap();

        // The simple existance of this test checks that `Backoff` doesn't
        // hang after seeing an error in `inner.poll_ready()`, but registers
        // to poll again.
        rt.block_on(future::poll_fn(|| backoff.poll_ready())).unwrap();
    }
}


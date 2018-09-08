use std::fmt;

use futures::{task, Async, Future, Poll};
use tower_reconnect;

use super::{NewService, Service};

/// Wraps `tower_reconnect`, handling errors.
///
/// Ensures that the underlying service is ready and, if the underlying service
/// fails to become ready, rebuilds the inner stack.
pub struct Reconnect<T, N>
where
    T: fmt::Debug,
    N: NewService,
{
    inner: tower_reconnect::Reconnect<N>,

    /// The target, used for debug logging.
    target: T,

    /// Prevents logging repeated connect errors.
    ///
    /// Set back to false after a connect succeeds, to log about future errors.
    mute_connect_error_log: bool,
}

pub struct ResponseFuture<N: NewService> {
    inner: <tower_reconnect::Reconnect<N> as Service>::Future,
}

// ===== impl Reconnect =====

impl<T, N> Reconnect<T, N>
where
    T: fmt::Debug,
    N: NewService,
    N::InitError: fmt::Display,
{
    pub fn new(target: T, new_service: N) -> Self {
        let inner = tower_reconnect::Reconnect::new(new_service);
        Self {
            target,
            inner,
            mute_connect_error_log: false,
        }
    }
}

impl<T, N> Service for Reconnect<T, N>
where
    T: fmt::Debug,
    N: NewService,
    N::InitError: fmt::Display,
{
    type Request = N::Request;
    type Response = N::Response;
    type Error = N::Error;
    type Future = ResponseFuture<N>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.inner.poll_ready() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(ready) => {
                trace!("poll_ready: ready for business");
                self.mute_connect_error_log = false;
                Ok(ready)
            }

            Err(tower_reconnect::Error::Inner(err)) => {
                trace!("poll_ready: inner error, debouncing");
                self.mute_connect_error_log = false;
                Err(err)
            }

            Err(tower_reconnect::Error::Connect(err)) => {
                // A connection could not be established to the target.

                // This is only logged as a warning at most once. Subsequent
                // errors are logged at debug.
                if !self.mute_connect_error_log {
                    self.mute_connect_error_log = true;
                    warn!("connect error to {:?}: {}", self.target, err);
                } else {
                    debug!("connect error to {:?}: {}", self.target, err);
                }

                // The inner service is now idle and will renew its internal
                // state on the next poll. Instead of doing this immediately,
                // the task is scheduled to be polled again only if the caller
                // decides not to drop it.
                //
                // This prevents busy-looping when the connect error is
                // instantaneous.
                task::current().notify();
                Ok(Async::NotReady)
            }

            Err(tower_reconnect::Error::NotReady) => {
                unreachable!("poll_ready can't fail with NotReady");
            }
        }
    }

    fn call(&mut self, request: Self::Request) -> Self::Future {
        ResponseFuture {
            inner: self.inner.call(request),
        }
    }
}

impl<N: NewService> Future for ResponseFuture<N> {
    type Item = N::Response;
    type Error = N::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll().map_err(|e| match e {
            tower_reconnect::Error::Inner(err) => err,
            _ => unreachable!("response future must fail with inner error"),
        })
    }
}

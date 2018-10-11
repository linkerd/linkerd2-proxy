use futures::{task, Async, Future, Poll};
use std::fmt;
use std::marker::PhantomData;
use tower_reconnect;

use svc;

#[derive(Debug)]
pub struct Layer<T, M> {
    _p: PhantomData<fn() -> (T, M)>,
}

#[derive(Debug)]
pub struct Stack<T, M> {
    inner: M,
    _p: PhantomData<fn() -> T>,
}

/// Wraps `tower_reconnect`, handling errors.
///
/// Ensures that the underlying service is ready and, if the underlying service
/// fails to become ready, rebuilds the inner stack.
pub struct Service<T, N>
where
    T: fmt::Debug,
    N: svc::NewService,
{
    inner: tower_reconnect::Reconnect<N>,

    /// The target, used for debug logging.
    target: T,

    /// Prevents logging repeated connect errors.
    ///
    /// Set back to false after a connect succeeds, to log about future errors.
    mute_connect_error_log: bool,
}

pub struct ResponseFuture<N: svc::NewService> {
    inner: <tower_reconnect::Reconnect<N> as svc::Service>::Future,
}

// === impl Layer ===

impl<T, M> Layer<T, M>
where
    T: fmt::Debug,
    M: svc::Stack<T>,
    M::Value: svc::NewService,
{
    pub fn new() -> Self {
        Self {
            _p: PhantomData,
        }
    }
}

impl<T, M> Clone for Layer<T, M> {
    fn clone(&self) -> Self {
        Self {
            _p: PhantomData,
        }
    }
}

impl<T, M> svc::Layer<T, T, M> for Layer<T, M>
where
    T: Clone + fmt::Debug,
    M: svc::Stack<T>,
    M::Value: svc::NewService,
{
    type Value = <Stack<T, M> as svc::Stack<T>>::Value;
    type Error = <Stack<T, M> as svc::Stack<T>>::Error;
    type Stack = Stack<T, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            _p: PhantomData,
        }
    }
}

// === impl Stack ===

impl<T, M: Clone> Clone for Stack<T, M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, M> svc::Stack<T> for Stack<T, M>
where
    T: Clone + fmt::Debug,
    M: svc::Stack<T>,
    M::Value: svc::NewService,
{
    type Value = Service<T, M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let new_service = self.inner.make(target)?;
        Ok(Service {
            inner: tower_reconnect::Reconnect::new(new_service),
            target: target.clone(),
            mute_connect_error_log: false,
        })
    }
}

// === impl Service ===

impl<T, N> svc::Service for Service<T, N>
where
    T: fmt::Debug,
    N: svc::NewService,
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
                self.mute_connect_error_log = false;
                Ok(ready)
            }

            Err(tower_reconnect::Error::Inner(err)) => {
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

impl<T: fmt::Debug, N: svc::NewService> fmt::Debug for Service<T, N> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Reconnect")
            .field("target", &self.target)
            .finish()
    }
}

impl<N: svc::NewService> Future for ResponseFuture<N> {
    type Item = N::Response;
    type Error = N::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll().map_err(|e| match e {
            tower_reconnect::Error::Inner(err) => err,
            _ => unreachable!("response future must fail with inner error"),
        })
    }
}

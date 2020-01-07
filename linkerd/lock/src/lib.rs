//! A middleware for sharing an inner service via mutual exclusion.

#![deny(warnings, rust_2018_idioms)]

use futures::{future, Async, Future, Poll};
use linkerd2_error::Error;
use tokio::sync::lock;
use tracing::trace;

#[derive(Clone, Debug)]
pub struct Layer<E = Poisoned> {
    _marker: std::marker::PhantomData<E>,
}

/// Guards access to an inner service with a `tokio::sync::lock::Lock`.
///
/// As the service is polled to readiness, the lock is acquired and the inner
/// service is polled. If the sevice is cloned, the service's lock state is not
/// retained by the clone.
///
/// The inner service's errors are coerced to the cloneable `C`-typed error so
/// that the error may be returned to all clones of the lock. By default, errors
/// are propagated through the `Poisoned` type, but they may be propagated
/// through custom types as well.
pub struct Lock<S, E = Poisoned> {
    lock: lock::Lock<State<S, E>>,
    locked: Option<lock::LockGuard<State<S, E>>>,
}

enum State<S, E> {
    Service(S),
    Error(E),
}

/// Propagates the inner error message to consumers.
#[derive(Clone, Debug)]
pub struct Poisoned(String);

// === impl Layer ===

impl Default for Layer {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> Layer<E>
where
    E: Clone + From<Error> + Into<Error>,
{
    /// Sets the error type to be returned to consumers when poll_ready fails.
    pub fn new() -> Self {
        Self {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<S, E: From<Error> + Clone> tower::layer::Layer<S> for Layer<E> {
    type Service = Lock<S, E>;

    fn layer(&self, service: S) -> Self::Service {
        Self::Service {
            locked: None,
            lock: lock::Lock::new(State::Service(service)),
        }
    }
}

// === impl Lock ===

impl<S, E> Clone for Lock<S, E> {
    fn clone(&self) -> Self {
        Self {
            locked: None,
            lock: self.lock.clone(),
        }
    }
}

impl<T, S, E> tower::Service<T> for Lock<S, E>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
    E: Clone + From<Error> + Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            if let Some(state) = self.locked.as_mut() {
                // Drive the service to readiness if it'is already locked.
                if let State::Service(ref mut inner) = **state {
                    return match inner.poll_ready() {
                        Ok(ok) => {
                            trace!(ready = ok.is_ready(), "service");
                            Ok(ok)
                        }
                        Err(inner) => {
                            // Coerce the error into `E` and clone it into the
                            // locked state so that it can be returned from all
                            // clones of the lock.
                            let err = E::from(inner.into());
                            **state = State::Error(err.clone());
                            self.locked = None;
                            Err(err.into())
                        }
                    };
                }

                // If an error occured above,the locked state is dropped and
                // cannot be acquired again.
                unreachable!("must not lock on error");
            }

            // Acquire the inner service exclusively so that the service can be
            // driven to readiness.
            match self.lock.poll_lock() {
                Async::NotReady => {
                    trace!(locked = false);
                    return Ok(Async::NotReady);
                }
                Async::Ready(locked) => {
                    if let State::Error(ref e) = *locked {
                        return Err(e.clone().into());
                    }

                    trace!(locked = true);
                    self.locked = Some(locked);
                }
            }
        }
    }

    fn call(&mut self, t: T) -> Self::Future {
        if let Some(mut state) = self.locked.take() {
            if let State::Service(ref mut inner) = *state {
                return inner.call(t).map_err(Into::into);
            }
        }

        unreachable!("called before ready");
    }
}

// === impl Poisoned ===

impl From<Error> for Poisoned {
    fn from(e: Error) -> Self {
        Poisoned(e.to_string())
    }
}

impl std::fmt::Display for Poisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Poisoned {}

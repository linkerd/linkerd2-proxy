use crate::error::{Error, Poisoned, ServiceError};
use crate::shared::{Shared, Wait};
use futures::{future, Async, Future, Poll};
use std::sync::{Arc, Mutex};
use tracing::trace;

/// A middleware that safely shares an inner service among clones.
///
/// As the service is polled to readiness, the lock is acquired and the inner service is polled. If
/// the service is cloned, the service's lock state isnot retained by the clone.
pub struct Lock<S> {
    state: State<S>,
    shared: Arc<Mutex<Shared<S>>>,
}

/// The state of a single `Lock` consumer.
enum State<S> {
    /// This lock has not registered interest in the inner service.
    Released,

    /// This lock is interested in the inner service.
    Waiting(Wait),

    /// This lock instance has exclusive ownership of the inner service.
    Acquired(S),

    /// The inner service has failed.
    Failed(Arc<Error>),
}

// === impl Lock ===

impl<S> Lock<S> {
    pub fn new(service: S) -> Self {
        Self {
            state: State::Released,
            shared: Arc::new(Mutex::new(Shared::new(service))),
        }
    }
}

impl<S> Clone for Lock<S> {
    fn clone(&self) -> Self {
        Self {
            // Clones have an independent local lock state.
            state: State::Released,
            shared: self.shared.clone(),
        }
    }
}

impl<T, S> tower::Service<T> for Lock<S>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            trace!(state = ?self.state, "Polling");
            self.state = match self.state {
                // This instance has exclusive access to the inner service.
                State::Acquired(ref mut svc) => match svc.poll_ready() {
                    Ok(ok) => {
                        trace!(ready = ok.is_ready());
                        return Ok(ok);
                    }
                    Err(inner) => {
                        // If the inner service fails to become ready, share that error with all
                        // other consumers and update this lock's state to prevent tryingto acquire
                        // the shared state again.
                        let error = Arc::new(inner.into());
                        trace!(%error);
                        if let Ok(mut shared) = self.shared.lock() {
                            shared.fail(error.clone());
                        }
                        State::Failed(error)
                    }
                },

                // This instance has not registered interest in the lock.
                State::Released => match self.shared.lock() {
                    Err(_) => return Err(Poisoned::new().into()),
                    // First, try to acquire the lock without creating a waiter. If the lock isn't
                    // available, create a waiter and try again, registering interest.
                    Ok(mut shared) => match shared.try_acquire() {
                        Ok(None) => State::Waiting(Wait::default()),
                        Ok(Some(svc)) => State::Acquired(svc),
                        Err(error) => State::Failed(error),
                    },
                },

                // This instance is interested in the lock.
                State::Waiting(ref waiter) => match self.shared.lock() {
                    Err(_) => return Err(Poisoned::new().into()),
                    Ok(mut shared) => match shared.poll_acquire(waiter) {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(svc)) => State::Acquired(svc),
                        Err(error) => State::Failed(error),
                    },
                },

                // The inner service failed, so share that failure with all consumers.
                State::Failed(ref error) => return Err(ServiceError::new(error.clone()).into()),
            };
        }
    }

    fn call(&mut self, req: T) -> Self::Future {
        // The service must have been acquired by poll_ready. Reset this lock's
        // state so that it must reacquire the service via poll_ready.
        let mut svc = match std::mem::replace(&mut self.state, State::Released) {
            State::Acquired(svc) => svc,
            _ => panic!("Called before ready"),
        };

        let fut = svc.call(req);

        // Return the service to the shared state, notifying waiters as needed.
        //
        // The service is dropped if the inner mutex has been poisoned, and subsequent calls to
        // poll_ready will return a Poisoned error.
        if let Ok(mut shared) = self.shared.lock() {
            trace!("Releasing acquired lock after use");
            shared.release_and_notify(svc);
        }

        // The inner service's error type is *not* wrapped with a ServiceError.
        fut.map_err(Into::into)
    }
}

impl<S> Drop for Lock<S> {
    fn drop(&mut self) {
        let state = std::mem::replace(&mut self.state, State::Released);
        trace!(?state, "Dropping");
        match state {
            // If this lock was holding the service, return it back to the shared state so another
            // lock may acquire it. Waiters are notified.
            State::Acquired(service) => {
                if let Ok(mut shared) = self.shared.lock() {
                    shared.release_and_notify(service);
                }
            }

            State::Waiting(wait) => {
                if let Ok(mut shared) = self.shared.lock() {
                    shared.release_waiter(wait);
                }
            }

            // No state to cleanup.
            State::Released | State::Failed(_) => {}
        }
    }
}

impl<S> std::fmt::Debug for State<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Released => write!(f, "Released"),
            State::Acquired(_) => write!(f, "Acquired(..)"),
            State::Waiting(ref w) => write!(f, "Waiting({:?})", w),
            State::Failed(ref e) => write!(f, "Failed({:?})", e),
        }
    }
}

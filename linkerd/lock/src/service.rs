use crate::error::{Error, ServiceError};
use crate::{Guard, Lock};
use futures::{future, TryFutureExt};
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::trace;

/// A middleware that safely shares an inner service among clones.
///
/// As the service is polled to readiness, the lock is acquired and the inner service is polled. If
/// the service is cloned, the service's lock state isnot retained by the clone.
pub struct LockService<S> {
    lock: Lock<Result<S, ServiceError>>,
    guard: Option<Guard<Result<S, ServiceError>>>,
}

impl<S> LockService<S> {
    pub fn new(inner: S) -> Self {
        Self {
            lock: Lock::new(Ok(inner)),
            guard: None,
        }
    }
}

impl<S> Clone for LockService<S> {
    fn clone(&self) -> Self {
        Self {
            lock: self.lock.clone(),
            guard: None,
        }
    }
}

impl<T, S> tower::Service<T> for LockService<S>
where
    S: tower::Service<T> + 'static,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            trace!(acquired = self.guard.is_some());
            if let Some(guard) = self.guard.as_mut() {
                return match guard.as_mut() {
                    Err(err) => Poll::Ready(Err(err.clone().into())),
                    Ok(ref mut svc) => match svc.poll_ready(cx) {
                        Poll::Ready(Err(inner)) => {
                            let error = ServiceError::new(Arc::new(inner.into()));
                            **guard = Err(error.clone());

                            // Drop the guard.
                            self.guard = None;
                            Poll::Ready(Err(error.into()))
                        }
                        Poll::Pending => {
                            trace!(ready = false);
                            Poll::Pending
                        }
                        Poll::Ready(Ok(())) => {
                            trace!(ready = true);
                            debug_assert!(self.guard.is_some());
                            Poll::Ready(Ok(()))
                        }
                    },
                };
            }
            debug_assert!(self.guard.is_none());

            let guard = futures::ready!(self.lock.poll_acquire(cx));
            self.guard = Some(guard);
        }
    }

    fn call(&mut self, req: T) -> Self::Future {
        trace!("Calling");
        // The service must have been acquired by poll_ready. Reset this lock's
        // state so that it must reacquire the service via poll_ready.
        self.guard
            .take()
            .expect("Called before ready")
            .as_mut()
            .expect("Called before ready")
            .call(req)
            .map_err(Into::into)
    }
}

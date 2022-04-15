//! Conditionally reconnects with a pluggable recovery/backoff strategy.
#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use futures::{future, prelude::*, ready};
use linkerd_error::{Error, Recover};
use linkerd_stack::{layer, NewService, Service};
use std::task::{Context, Poll};
use tracing::{debug, trace, warn};

#[derive(Clone, Debug)]
pub struct NewReconnect<R, N> {
    recover: R,
    inner: N,
}

#[derive(Debug)]
pub struct Reconnect<T, R, N>
where
    R: Recover,
    N: NewService<T>,
{
    target: T,
    recover: R,
    inner: N,
    state: State<R::Backoff, N::Service>,
}

#[derive(Debug)]
enum State<B, S> {
    Disconnected {
        backoff: Option<B>,
    },
    Pending {
        service: Option<S>,
        backoff: Option<B>,
    },
    Connected(S),
}

// === impl NewReconnect ===

impl<R: Clone, N> NewReconnect<R, N> {
    pub fn new(recover: R, inner: N) -> Self {
        Self { inner, recover }
    }

    pub fn layer(recover: R) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(recover.clone(), inner))
    }
}

impl<T, R, N> NewService<T> for NewReconnect<R, N>
where
    R: Recover + Clone,
    N: NewService<T> + Clone,
{
    type Service = Reconnect<T, R, N>;

    fn new_service(&self, target: T) -> Self::Service {
        Reconnect::new(target, self.inner.clone(), self.recover.clone())
    }
}

// === impl Reconnect ===

impl<T, R, N> Reconnect<T, R, N>
where
    R: Recover,
    N: NewService<T>,
{
    pub fn new(target: T, inner: N, recover: R) -> Self {
        Self {
            target,
            inner,
            recover,
            state: State::Disconnected { backoff: None },
        }
    }
}

impl<T, Req, R, N, S> Service<Req> for Reconnect<T, R, N>
where
    T: Clone,
    R: Recover,
    R::Backoff: Unpin,
    N: NewService<T, Service = S>,
    S: Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            self.state = match self.state {
                // If the service is connected, ensure it's ready. If checking the readiness fails,
                // go into the disconnected state to poll the backoff before creating a new service.
                State::Connected(ref mut svc) => match ready!(svc.poll_ready(cx)) {
                    Ok(()) => {
                        trace!("Ready");
                        return Poll::Ready(Ok(()));
                    }
                    Err(e) => {
                        // If the service fails, try to recover.
                        let error: Error = e.into();
                        warn!(%error, "Service failed");
                        let backoff = self.recover.recover(error)?;
                        debug!("Recovering");
                        State::Disconnected {
                            backoff: Some(backoff),
                        }
                    }
                },

                // If the service has not yet become ready, check its readiness and, if that fails,
                // retain the prior backoff. Otherwise, mark the service as connected. We'll avoid
                // immediately re-polling the service below.
                State::Pending {
                    ref mut service,
                    ref mut backoff,
                } => match ready!(service.as_mut().unwrap().poll_ready(cx)) {
                    Ok(()) => {
                        debug!("Connected");
                        State::Connected(service.take().unwrap())
                    }
                    Err(e) => {
                        // If the service cannot be built, try to recover using
                        // the existing backoff.
                        let error: Error = e.into();
                        warn!(%error, "Failed to connect");
                        let new_backoff = self.recover.recover(error)?;
                        debug!("Recovering");
                        State::Disconnected {
                            backoff: Some(backoff.take().unwrap_or(new_backoff)),
                        }
                    }
                },

                // If the service is disconnected and there's already a backoff, wait for it. Then
                // create a new service and go into pending.
                State::Disconnected { ref mut backoff } => {
                    debug!(backoff = %backoff.is_some(), "Disconnected");
                    let backoff = if backoff.is_some() {
                        // Do not try to recover if the backoff stream fails.
                        let more = ready!(backoff
                            .as_mut()
                            .expect("backoff must be set")
                            .poll_next_unpin(cx));

                        // Only reuse the backoff if the backoff stream did not complete.
                        if more.is_some() {
                            backoff.take()
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // When disconnected, try to build a new inner service. If this call fails, we
                    // must not recover, since `new_service` is now unusable.
                    debug!(backoff = %backoff.is_some(), "Creating service");
                    let svc = self.inner.new_service(self.target.clone());
                    State::Pending {
                        service: Some(svc),
                        backoff,
                    }
                }
            };

            // If we just transitioned from pending to connected, avoid re-polling the inner
            // service.
            if let State::Connected(_) = self.state {
                return Poll::Ready(Ok(()));
            }
        }
    }

    #[inline]
    fn call(&mut self, request: Req) -> Self::Future {
        if let State::Connected(ref mut svc) = self.state {
            return svc.call(request).map_err(Into::into);
        }

        unreachable!("Called before ready");
    }
}

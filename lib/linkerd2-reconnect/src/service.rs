use futures::{future, try_ready, Async, Future, Poll, Stream};
use linkerd2_error::{Error, Recover};
use tracing;

pub struct Service<T, R, M>
where
    R: Recover,
    M: tower::Service<T>,
{
    target: T,
    recover: R,
    make_service: M,
    state: State<M::Future, R::Backoff>,
}

enum State<F: Future, B> {
    Disconnected {
        backoff: Option<B>,
    },
    Pending {
        future: F,
        backoff: Option<B>,
    },
    Service(F::Item),
    Recover {
        error: Option<Error>,
        backoff: Option<B>,
    },
    Backoff(Option<B>),
}

// === impl Service ===

impl<T, R, M> Service<T, R, M>
where
    T: Clone,
    R: Recover,
    M: tower::Service<T>,
    M::Error: Into<Error>,
{
    pub fn new(target: T, make_service: M, recover: R) -> Self {
        Self {
            target,
            recover,
            make_service,
            state: State::Disconnected { backoff: None },
        }
    }
}

impl<Req, T, R, M, S> tower::Service<Req> for Service<T, R, M>
where
    T: Clone,
    R: Recover,
    M: tower::Service<T, Response = S>,
    M::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            self.state = match self.state {
                State::Disconnected { ref mut backoff } => {
                    // When disconnected, try to build a new inner sevice. If
                    // this call fails, we must not recover, since
                    // `make_service` is now unusable.
                    try_ready!(self.make_service.poll_ready().map_err(Into::into));
                    State::Pending {
                        future: self.make_service.call(self.target.clone()),
                        backoff: backoff.take(),
                    }
                }

                State::Pending {
                    ref mut future,
                    ref mut backoff,
                } => match future.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(service)) => State::Service(service),
                    Err(e) => {
                        // If the service cannot be built, try to recover using
                        // the existing backoff.
                        let error: Error = e.into();
                        tracing::debug!(message="Failed to connect", %error);
                        State::Recover {
                            error: Some(error),
                            backoff: backoff.take(),
                        }
                    }
                },

                State::Service(ref mut service) => match service.poll_ready() {
                    Ok(ready) => return Ok(ready),
                    Err(e) => {
                        // If the service fails, try to recover.
                        let error: Error = e.into();
                        tracing::warn!(message="Service failed", %error);
                        State::Recover {
                            error: Some(error),
                            backoff: None,
                        }
                    }
                },

                State::Recover {
                    ref mut error,
                    ref mut backoff,
                } => {
                    // Apply the recovery strategy. If the error isn't fatal,
                    // prefer the existing backoff to the new one.
                    let error = error.take().expect("error must be set");
                    let new_backoff = self.recover.recover(error)?;
                    tracing::debug!("Recovering");
                    State::Backoff(Some(backoff.take().unwrap_or(new_backoff)))
                }

                State::Backoff(ref mut backoff) => {
                    // Do not try to recover if the backoff stream fails.
                    let more = try_ready!(backoff
                        .as_mut()
                        .expect("backoff must be set")
                        .poll()
                        .map_err(Into::into));
                    State::Disconnected {
                        // Only reuse the backoff if the backoff stream did not complete.
                        backoff: more.and_then(move |()| backoff.take()),
                    }
                }
            }
        }
    }

    #[inline]
    fn call(&mut self, request: Req) -> Self::Future {
        if let State::Service(ref mut service) = self.state {
            return service.call(request).map_err(Into::into);
        }

        unreachable!("Called before ready");
    }
}

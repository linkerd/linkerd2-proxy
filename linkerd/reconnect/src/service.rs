use futures::{future, prelude::*, ready};
use linkerd2_error::{Error, Recover};
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct Service<R, M>
where
    R: Recover,
    M: tower::Service<()>,
{
    recover: R,
    make_service: M,
    state: State<M::Future, R::Backoff>,
}

enum State<F: TryFuture, B> {
    Disconnected {
        backoff: Option<B>,
    },
    Pending {
        future: Pin<Box<F>>,
        backoff: Option<B>,
    },
    Service(F::Ok),
    Recover {
        error: Option<Error>,
        backoff: Option<B>,
    },
    Backoff(Option<B>),
}

// === impl Service ===

impl<R, M> Service<R, M>
where
    R: Recover,
    M: tower::Service<()>,
{
    pub(crate) fn new(recover: R, make_service: M) -> Self {
        Self {
            recover,
            make_service,
            state: State::Disconnected { backoff: None },
        }
    }
}

impl<Req, R, M, S> tower::Service<Req> for Service<R, M>
where
    R: Recover,
    R::Backoff: Unpin,
    M: tower::Service<(), Response = S>,
    M::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            self.state = match self.state {
                State::Disconnected { ref mut backoff } => {
                    // When disconnected, try to build a new inner sevice. If
                    // this call fails, we must not recover, since
                    // `make_service` is now unusable.
                    ready!(self.make_service.poll_ready(cx)).map_err(Into::into)?;
                    State::Pending {
                        future: Box::pin(self.make_service.call(())),
                        backoff: backoff.take(),
                    }
                }

                State::Pending {
                    ref mut future,
                    ref mut backoff,
                } => match ready!(Pin::new(future).try_poll(cx)) {
                    Ok(service) => State::Service(service),
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

                State::Service(ref mut service) => match ready!(service.poll_ready(cx)) {
                    Err(e) => {
                        // If the service fails, try to recover.
                        let error: Error = e.into();
                        tracing::warn!(message="Service failed", %error);
                        State::Recover {
                            error: Some(error),
                            backoff: None,
                        }
                    }
                    Ok(ready) => return Poll::Ready(Ok(ready)),
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
                    let more = ready!(backoff
                        .as_mut()
                        .expect("backoff must be set")
                        .poll_next_unpin(cx));

                    // Only reuse the backoff if the backoff stream did not complete.
                    let backoff = if more.is_some() { backoff.take() } else { None };

                    State::Disconnected { backoff }
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

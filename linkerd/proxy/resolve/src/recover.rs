//! A middleware that recovers a resolution after some failures.

use futures::stream::TryStream;
use futures::{prelude::*, ready, FutureExt, Stream};
use linkerd_error::{Error, Recover};
use linkerd_proxy_core::resolve::{self, Update};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

#[derive(Clone, Debug, Error)]
#[error("end of stream reached")]
pub struct Eos(());

#[derive(Clone, Debug)]
pub struct Resolve<E, R> {
    resolve: R,
    recover: E,
}

#[pin_project]
pub struct ResolveFuture<T, E: Recover, R: resolve::Resolve<T>> {
    inner: Option<Inner<T, E, R>>,
}

#[pin_project(project = ResolutionProj)]
pub struct Resolution<T, E: Recover, R: resolve::Resolve<T>> {
    inner: Inner<T, E, R>,
}

#[pin_project]
struct Inner<T, E: Recover, R: resolve::Resolve<T>> {
    target: T,
    resolve: R,
    recover: E,
    state: State<R::Future, R::Resolution, E::Backoff>,
}

#[pin_project]
enum State<F, R: TryStream, B> {
    Disconnected {
        backoff: Option<B>,
    },
    Connecting {
        future: F,
        backoff: Option<B>,
    },

    Connected {
        #[pin]
        resolution: R,
        is_initial: bool,
    },

    Recover {
        error: Option<Error>,
        backoff: Option<B>,
    },

    Backoff(Option<B>),
}

// === impl Resolve ===

impl<E, R> Resolve<E, R> {
    pub fn new(recover: E, resolve: R) -> Self {
        Self { resolve, recover }
    }
}

impl<T, E, R> tower::Service<T> for Resolve<E, R>
where
    T: Clone,
    R: resolve::Resolve<T> + Clone,
    R::Resolution: Unpin,
    R::Future: Unpin,
    E: Recover + Clone,
    E::Backoff: Unpin,
{
    type Response = Resolution<T, E, R>;
    type Error = Error;
    type Future = ResolveFuture<T, E, R>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        let future = self.resolve.resolve(target.clone());

        Self::Future {
            inner: Some(Inner {
                state: State::Connecting {
                    future,
                    backoff: None,
                },
                target,
                recover: self.recover.clone(),
                resolve: self.resolve.clone(),
            }),
        }
    }
}

// === impl ResolveFuture ===

impl<T, E, R> Future for ResolveFuture<T, E, R>
where
    T: Clone,
    R: resolve::Resolve<T>,
    R::Resolution: Unpin,
    R::Future: Unpin,
    E: Recover,
    E::Backoff: Unpin,
{
    type Output = Result<Resolution<T, E, R>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        // Wait until the resolution is connected.
        ready!(this
            .inner
            .as_mut()
            .expect("polled after complete")
            .poll_connected(cx))?;
        let inner = this.inner.take().expect("polled after complete");
        Poll::Ready(Ok(Resolution { inner }))
    }
}

// === impl Resolution ===

impl<T, E, R> Stream for Resolution<T, E, R>
where
    T: Clone,
    R: resolve::Resolve<T>,
    R::Future: Unpin,
    R::Resolution: Unpin,
    R::Endpoint: std::fmt::Debug,
    E: Recover,
    E::Backoff: Unpin,
{
    type Item = Result<Update<R::Endpoint>, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            // XXX(eliza): note that this match was originally an `if let`,
            // but that doesn't work with `#[project]` for some kinda reason
            #[allow(clippy::single_match)]
            match this.inner.state {
                State::Connected {
                    ref mut resolution,
                    ref mut is_initial,
                } => {
                    tracing::trace!("polling");
                    match ready!(resolution.try_poll_next_unpin(cx)) {
                        Some(Ok(Update::Remove(_))) if *is_initial => {
                            debug_assert!(false, "Remove must not be initial update");
                            tracing::debug!("Ignoring Remove after connection");
                            // Continue polling until a useful update is received.
                        }
                        Some(Ok(update)) => {
                            let update = if *is_initial {
                                *is_initial = false;
                                match update {
                                    Update::Add(eps) => Update::Reset(eps),
                                    up => up,
                                }
                            } else {
                                update
                            };
                            tracing::trace!(?update);
                            return Poll::Ready(Some(Ok(update)));
                        }
                        Some(Err(e)) => {
                            this.inner.state = State::Recover {
                                error: Some(e.into()),
                                backoff: None,
                            }
                        }
                        None => {
                            this.inner.state = State::Recover {
                                error: Some(Eos(()).into()),
                                backoff: None,
                            }
                        }
                    }
                }
                _ => {}
            }

            ready!(this.inner.poll_connected(cx))?;
        }
    }
}

// === impl Inner ===

impl<T, E, R> Inner<T, E, R>
where
    T: Clone,
    R: resolve::Resolve<T>,
    R::Resolution: Unpin,
    R::Future: Unpin,
    E: Recover,
    E::Backoff: Unpin,
{
    /// Drives the state forward until its connected.
    fn poll_connected(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        loop {
            self.state = match self.state {
                // When disconnected, start connecting.
                //
                // If we're recovering from a previous failure, we retain the
                // backoff in case this connection attempt fails.
                State::Disconnected { ref mut backoff } => {
                    tracing::trace!("connecting");
                    let future = self.resolve.resolve(self.target.clone());
                    let backoff = backoff.take();
                    State::Connecting { future, backoff }
                }

                State::Connecting {
                    ref mut future,
                    ref mut backoff,
                } => match ready!(future.poll_unpin(cx)) {
                    Ok(resolution) => {
                        tracing::trace!("Connected");
                        State::Connected {
                            resolution,
                            is_initial: true,
                        }
                    }
                    Err(e) => State::Recover {
                        error: Some(e.into()),
                        backoff: backoff.take(),
                    },
                },

                State::Connected { .. } => return Poll::Ready(Ok(())),

                // If any stage failed, try to recover. If the error is
                // recoverable, start (or continue) backing off...
                State::Recover {
                    ref mut error,
                    ref mut backoff,
                } => {
                    let err = error.take().expect("illegal state");
                    tracing::debug!(%err, "recovering");
                    let new_backoff = self.recover.recover(err)?;
                    State::Backoff(backoff.take().or(Some(new_backoff)))
                }

                State::Backoff(ref mut backoff) => {
                    let more = ready!(backoff.as_mut().expect("illegal state").poll_next_unpin(cx));
                    let backoff = if more.is_some() { backoff.take() } else { None };
                    tracing::trace!("disconnected");
                    State::Disconnected { backoff }
                }
            };
        }
    }
}

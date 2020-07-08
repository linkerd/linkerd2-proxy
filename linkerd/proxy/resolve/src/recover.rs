//! A middleware that recovers a resolution after some failures.

use futures::{ready, stream::TryStreamExt, FutureExt};
use indexmap::IndexMap;
use linkerd2_error::{Error, Recover};
use linkerd2_proxy_core::resolve::{self, Resolution as _, Update};
use pin_project::pin_project;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

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
    cache: IndexMap<SocketAddr, R::Endpoint>,
    reconcile: Option<Update<R::Endpoint>>,
}

#[pin_project]
struct Inner<T, E: Recover, R: resolve::Resolve<T>> {
    target: T,
    resolve: R,
    recover: E,
    state: State<R::Future, R::Resolution, E::Backoff>,
}

#[derive(Debug)]
struct Cache<T> {
    active: IndexMap<SocketAddr, T>,
}

#[pin_project]
enum State<F, R: resolve::Resolution, B> {
    Disconnected {
        backoff: Option<B>,
    },
    Connecting {
        future: F,
        backoff: Option<B>,
    },

    // XXX This state shouldn't be necessary, but we need it to pass tests(!)
    // that don't properly mimic the go server's behavior. See
    // linkerd/linkerd2#3362.
    Pending {
        resolution: Option<R>,
        backoff: Option<B>,
    },

    Connected {
        #[pin]
        resolution: R,
        initial: Option<Update<R::Endpoint>>,
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
    R::Endpoint: Clone + PartialEq,
    E: Recover + Clone,
    E::Backoff: Unpin,
{
    type Response = Resolution<T, E, R>;
    type Error = Error;
    type Future = ResolveFuture<T, E, R>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.resolve.poll_ready(cx).map_err(Into::into)
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
                target: target.clone(),
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
    R::Endpoint: Clone + PartialEq,
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
        Poll::Ready(Ok(Resolution {
            inner,
            cache: IndexMap::default(),
            reconcile: None,
        }))
    }
}

// === impl Resolution ===

impl<T, E, R> resolve::Resolution for Resolution<T, E, R>
where
    T: Clone,
    R: resolve::Resolve<T>,
    R::Future: Unpin,
    R::Resolution: Unpin,
    R::Endpoint: Clone + PartialEq,
    E: Recover,
    E::Backoff: Unpin,
{
    type Endpoint = R::Endpoint;
    type Error = Error;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Update<Self::Endpoint>, Self::Error>> {
        let mut this = self.project();
        loop {
            // If a reconciliation update is buffered (i.e. after
            // reconcile_after_reconnect), process it immediately.
            if let Some(update) = this.reconcile.take() {
                this.update_active(&update);
                return Poll::Ready(Ok(update));
            }

            match this.inner.state {
                State::Connected {
                    ref mut resolution,
                    ref mut initial,
                } => {
                    // XXX Due to linkerd/linkerd2#3362, errors can't be discovered
                    // eagerly, so we must potentially read the first update to be
                    // sure it didn't fail. If that's the case, then reconcile the
                    // cache against the initial update.
                    if let Some(initial) = initial.take() {
                        // The initial state afer a reconnect may be identitical to
                        // the prior state, and so there may be no updates to
                        // advertise.
                        if let Some((update, reconcile)) =
                            reconcile_after_connect(&this.cache, initial)
                        {
                            *this.reconcile = reconcile;
                            this.update_active(&update);
                            return Poll::Ready(Ok(update));
                        }
                    }

                    // Process the resolution stream, updating the cache.
                    //
                    // Attempt recovery/backoff if the resolution fails.
                    match ready!(resolution.poll_unpin(cx)) {
                        Ok(update) => {
                            this.update_active(&update);
                            return Poll::Ready(Ok(update));
                        }
                        Err(e) => {
                            this.inner.state = State::Recover {
                                error: Some(e.into()),
                                backoff: None,
                            }
                        }
                    }
                }
                // XXX(eliza): note that this match was originally an `if let`,
                // but that doesn't work with `#[project]` for some kinda reason
                _ => {}
            }

            ready!(this.inner.poll_connected(cx))?;
        }
    }
}

impl<T, E, R> ResolutionProj<'_, T, E, R>
where
    T: Clone,
    R: resolve::Resolve<T>,
    R::Endpoint: Clone + PartialEq,
    E: Recover,
{
    fn update_active(&mut self, update: &Update<R::Endpoint>) {
        match update {
            Update::Add(ref endpoints) => {
                self.cache.extend(endpoints.clone());
            }
            Update::Remove(ref addrs) => {
                for addr in addrs.iter() {
                    self.cache.remove(addr);
                }
            }
            Update::DoesNotExist | Update::Empty => {
                self.cache.drain(..);
            }
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
    R::Endpoint: Clone + PartialEq,
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
                    ready!(self.resolve.poll_ready(cx).map_err(Into::into))?;
                    let future = self.resolve.resolve(self.target.clone());
                    let backoff = backoff.take();
                    State::Connecting { future, backoff }
                }

                State::Connecting {
                    ref mut future,
                    ref mut backoff,
                } => match ready!(future.poll_unpin(cx)) {
                    Ok(resolution) => {
                        tracing::trace!("pending");
                        State::Pending {
                            resolution: Some(resolution),
                            backoff: backoff.take(),
                        }
                    }
                    Err(e) => State::Recover {
                        error: Some(e.into()),
                        backoff: backoff.take(),
                    },
                },

                // We've already connected, but haven't yet received an update
                // (or an error). This state shouldn't exist. See
                // linkerd/linkerd2#3362.
                State::Pending {
                    ref mut resolution,
                    ref mut backoff,
                } => match ready!(resolution.as_mut().expect("illegal state").poll_unpin(cx)) {
                    Err(e) => State::Recover {
                        error: Some(e.into()),
                        backoff: backoff.take(),
                    },
                    Ok(initial) => {
                        tracing::trace!("connected");
                        State::Connected {
                            resolution: resolution.take().expect("illegal state"),
                            initial: Some(initial),
                        }
                    }
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
                    let unit = ready!(backoff
                        .as_mut()
                        .expect("illegal state")
                        .try_poll_next_unpin(cx));
                    tracing::trace!("disconnected");
                    let backoff = if let Some(unit) = unit {
                        // If the backoff fails, it's not recoverable.
                        unit.map_err(Into::into)?;
                        backoff.take()
                    } else {
                        None
                    };
                    State::Disconnected { backoff }
                }
            };
        }
    }
}

/// Computes the updates needed after a connection is (re-)established.
// Raw fn for easier testing.
fn reconcile_after_connect<E: PartialEq>(
    cache: &IndexMap<SocketAddr, E>,
    initial: Update<E>,
) -> Option<(Update<E>, Option<Update<E>>)> {
    match initial {
        // When the first update after a disconnect is an Add, it should
        // contain the new state of the replica set.
        Update::Add(endpoints) => {
            let mut new_eps = endpoints.into_iter().collect::<IndexMap<_, _>>();
            let mut rm_addrs = Vec::with_capacity(cache.len());
            for (addr, endpoint) in cache.iter() {
                match new_eps.get(addr) {
                    // If the endpoint is in the active set and not in
                    // the new set, it needs to be removed.
                    None => {
                        rm_addrs.push(*addr);
                    }
                    // If the endpoint is already in the active set,
                    // remove it from the new set (to avoid rebuilding
                    // services unnecessarily).
                    Some(ep) => {
                        // The endpoints must be identitical, though.
                        if *ep == *endpoint {
                            new_eps.remove(addr);
                        }
                    }
                }
            }
            let add = if new_eps.is_empty() {
                None
            } else {
                Some(Update::Add(new_eps.into_iter().collect()))
            };
            let rm = if rm_addrs.is_empty() {
                None
            } else {
                Some(Update::Remove(rm_addrs))
            };
            // Advertise adds before removes so that we don't unnecessarily
            // empty out a consumer.
            match add {
                Some(add) => Some((add, rm)),
                None => rm.map(|rm| (rm, None)),
            }
        }
        // It would be exceptionally odd to get a remove, specifically,
        // immediately after a reconnect, but it seems appropriate to
        // handle it as Empty.
        Update::Remove(..) | Update::Empty => Some((Update::Empty, None)),
        Update::DoesNotExist => Some((Update::DoesNotExist, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn addr0() -> SocketAddr {
        ([198, 51, 100, 1], 8080).into()
    }

    pub fn addr1() -> SocketAddr {
        ([198, 51, 100, 2], 8080).into()
    }

    #[test]
    fn reconcile_after_initial_connect() {
        let cache = IndexMap::default();
        let add = Update::Add(vec![(addr0(), 0), (addr1(), 0)]);
        assert_eq!(
            reconcile_after_connect(&cache, add.clone()),
            Some((add, None)),
            "Adds should be passed through initially"
        );
        assert_eq!(
            reconcile_after_connect(&cache, Update::Remove(vec![addr0(), addr1()])),
            Some((Update::Empty, None)),
            "Removes should be treated as empty"
        );
        assert_eq!(
            reconcile_after_connect(&cache, Update::Empty),
            Some((Update::Empty, None)),
            "Empties should be passed through"
        );
        assert_eq!(
            reconcile_after_connect(&cache, Update::DoesNotExist),
            Some((Update::DoesNotExist, None)),
            "DNEs should be passed through"
        );
    }

    #[test]
    fn reconcile_after_reconnect_dedupes() {
        let mut cache = IndexMap::new();
        cache.insert(addr0(), 0);

        assert_eq!(
            reconcile_after_connect(&cache, Update::Add(vec![(addr0(), 0), (addr1(), 0)])),
            Some((Update::Add(vec![(addr1(), 0)]), None)),
        );
    }

    #[test]
    fn reconcile_after_reconnect_updates() {
        let mut cache = IndexMap::new();
        cache.insert(addr0(), 0);

        assert_eq!(
            reconcile_after_connect(&cache, Update::Add(vec![(addr0(), 1), (addr1(), 0)])),
            Some((Update::Add(vec![(addr0(), 1), (addr1(), 0)]), None)),
        );
    }

    #[test]
    fn reconcile_after_reconnect_removes() {
        let mut cache = IndexMap::new();
        cache.insert(addr0(), 0);
        cache.insert(addr1(), 0);

        assert_eq!(
            reconcile_after_connect(&cache, Update::Add(vec![(addr0(), 0)])),
            Some((Update::Remove(vec![addr1()]), None))
        );
    }

    #[test]
    fn reconcile_after_reconnect_adds_and_removes() {
        let mut cache = IndexMap::new();
        cache.insert(addr0(), 0);
        cache.insert(addr1(), 0);

        assert_eq!(
            reconcile_after_connect(&cache, Update::Add(vec![(addr0(), 1)])),
            Some((
                Update::Add(vec![(addr0(), 1)]),
                Some(Update::Remove(vec![addr1()]))
            ))
        );
    }

    #[test]
    fn reconcile_after_reconnect_passthru() {
        let mut cache = IndexMap::default();
        cache.insert(addr0(), 0);

        assert_eq!(
            reconcile_after_connect(&cache, Update::Remove(vec![addr1()])),
            Some((Update::Empty, None)),
            "Removes should be treated as empty"
        );
        assert_eq!(
            reconcile_after_connect(&cache, Update::Empty),
            Some((Update::Empty, None)),
            "Empties should be passed through"
        );
        assert_eq!(
            reconcile_after_connect(&cache, Update::DoesNotExist),
            Some((Update::DoesNotExist, None)),
            "DNEs should be passed through"
        );
    }
}

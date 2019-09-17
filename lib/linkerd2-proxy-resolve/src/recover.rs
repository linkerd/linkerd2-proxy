//! A middleware that recovers a resolution after some failures.

use futures::{try_ready, Async, Future, Poll, Stream};
use indexmap::IndexMap;
use linkerd2_error::{Error, Recover};
use linkerd2_proxy_core::resolve::{self, Resolution as _, Update};
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct Resolve<E, R> {
    resolve: R,
    recover: E,
}

pub struct ResolveFuture<T, E: Recover, R: resolve::Resolve<T>> {
    inner: Option<Inner<T, E, R>>,
}

pub struct Resolution<T, E: Recover, R: resolve::Resolve<T>> {
    inner: Inner<T, E, R>,
    cache: IndexMap<SocketAddr, R::Endpoint>,
    reconcile: Option<Update<R::Endpoint>>,
}

struct Inner<T, E: Recover, R: resolve::Resolve<T>> {
    target: T,
    resolve: R,
    recover: E,
    state: State<R::Future, R::Resolution, E::Backoff>,
}

// #[derive(Debug)]
// struct Cache<T> {
//     active: IndexMap<SocketAddr, T>,
// }

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
    pub fn new<T>(recover: E, resolve: R) -> Self
    where
        Self: resolve::Resolve<T>,
    {
        Self { resolve, recover }
    }
}

impl<T, E, R> tower::Service<T> for Resolve<E, R>
where
    T: Clone,
    R: resolve::Resolve<T> + Clone,
    R::Endpoint: Clone + PartialEq,
    E: Recover + Clone,
{
    type Response = Resolution<T, E, R>;
    type Error = Error;
    type Future = ResolveFuture<T, E, R>;

    #[inline]
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.resolve.poll_ready().map_err(Into::into)
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
    R::Endpoint: Clone + PartialEq,
    E: Recover,
{
    type Item = Resolution<T, E, R>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Wait until the resolution is connected.
        try_ready!(self
            .inner
            .as_mut()
            .expect("polled after complete")
            .poll_connected());

        Ok(Async::Ready(Resolution {
            inner: self.inner.take().expect("polled after complete"),
            cache: IndexMap::default(),
            //cache: Cache::default(),
            reconcile: None,
        }))
    }
}

// === impl Resolution ===

impl<T, E, R> resolve::Resolution for Resolution<T, E, R>
where
    T: Clone,
    R: resolve::Resolve<T>,
    R::Endpoint: Clone + PartialEq,
    E: Recover,
{
    type Endpoint = R::Endpoint;
    type Error = Error;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        loop {
            // If a reconciliation update is buffered (i.e. after
            // reconcile_after_reconnect), process it immediately.
            if let Some(update) = self.reconcile.take() {
                self.update_active(&update);
                return Ok(update.into());
            }

            if let State::Connected {
                ref mut resolution,
                ref mut initial,
            } = self.inner.state
            {
                // XXX Due to linkerd/linkerd2#3362, errors can't be discovered
                // eagerly, so we must potentially read the first update to be
                // sure it didn't fail. If that's the case, then reconcile the
                // cache against the initial update.
                if let Some(initial) = initial.take() {
                    // The initial state afer a reconnect may be identitical to
                    // the prior state, and so there may be no updates to
                    // advertise.
                    if let Some((update, reconcile)) = reconcile_after_connect(&self.cache, initial)
                    {
                        self.reconcile = reconcile;
                        self.update_active(&update);
                        return Ok(update.into());
                    }
                }

                // Process the resolution stream, updating the cache.
                //
                // Attempt recovery/backoff if the resolution fails.
                match resolve::Resolution::poll(resolution) {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(update)) => {
                        self.update_active(&update);
                        return Ok(update.into());
                    }
                    Err(e) => {
                        self.inner.state = State::Recover {
                            error: Some(e.into()),
                            backoff: None,
                        };
                    }
                }
            }

            try_ready!(self.inner.poll_connected());
        }
    }
}

impl<T, E, R> Resolution<T, E, R>
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
    R::Endpoint: Clone + PartialEq,
    E: Recover,
{
    /// Drives the state forward until its connected.
    fn poll_connected(&mut self) -> Poll<(), Error> {
        loop {
            self.state = match self.state {
                // When disconnected, start connecting.
                //
                // If we're recovering from a previous failure, we retain the
                // backoff in case this connection attempt fails.
                State::Disconnected { ref mut backoff } => {
                    tracing::trace!("connecting");
                    let future = self.resolve.resolve(self.target.clone());
                    State::Connecting {
                        future,
                        backoff: backoff.take(),
                    }
                }

                State::Connecting {
                    ref mut future,
                    ref mut backoff,
                } => match future.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(resolution)) => {
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
                } => match resolution.as_mut().unwrap().poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(e) => State::Recover {
                        error: Some(e.into()),
                        backoff: backoff.take(),
                    },
                    Ok(Async::Ready(initial)) => {
                        tracing::trace!("connected");
                        State::Connected {
                            resolution: resolution.take().unwrap(),
                            initial: Some(initial),
                        }
                    }
                },

                State::Connected { .. } => return Ok(Async::Ready(())),

                // If any stage failed, try to recover. If the error is
                // recoverable, start (or continue) backing off...
                State::Recover {
                    ref mut error,
                    ref mut backoff,
                } => {
                    let err = error.take().expect("illegal state");
                    tracing::debug!(message = %err);
                    let new_backoff = self.recover.recover(err)?;
                    State::Backoff(backoff.take().or(Some(new_backoff)))
                }

                State::Backoff(ref mut backoff) => {
                    // If the backoff fails, it's not recoverable.
                    match backoff
                        .as_mut()
                        .expect("illegal state")
                        .poll()
                        .map_err(Into::into)?
                    {
                        Async::NotReady => return Ok(Async::NotReady),
                        Async::Ready(unit) => {
                            tracing::trace!("disconnected");
                            let backoff = if unit.is_some() { backoff.take() } else { None };
                            State::Disconnected { backoff }
                        }
                    }
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

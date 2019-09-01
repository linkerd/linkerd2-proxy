//! A middleware that recovers a resolution after some failures.

use futures::{try_ready, Async, Future, Poll, Stream};
use indexmap::IndexMap;
use linkerd2_proxy_core::resolve::{self, Update};
use linkerd2_proxy_core::{Error, Recover};
use std::fmt;
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
    cache: Cache<R::Endpoint>,
}

struct Inner<T, E: Recover, R: resolve::Resolve<T>> {
    target: T,
    resolve: R,
    recover: E,
    state: State<R::Future, R::Resolution, E::Backoff>,
}

#[derive(Debug)]
struct Cache<T> {
    pending_add: IndexMap<SocketAddr, T>,
    active: IndexMap<SocketAddr, T>,
}

enum State<F, R: resolve::Resolution, B> {
    Disconnected {
        backoff: Option<B>,
    },
    Connecting {
        future: F,
        backoff: Option<B>,
    },
    Connected {
        resolution: R,
        connection: Connection,
    },
    Failed {
        error: Option<Error>,
        backoff: Option<B>,
    },
    Backoff(Option<B>),
}

#[derive(Debug)]
enum Connection {
    Reconnected,
    Established,
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
    T: fmt::Display + Clone,
    R: resolve::Resolve<T> + Clone,
    R::Endpoint: Clone + PartialEq,
    E: Recover + Clone,
{
    type Response = Resolution<T, E, R>;
    type Error = Error;
    type Future = ResolveFuture<T, E, R>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.resolve.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.resolve.resolve(target.clone());
        let inner = Inner {
            target: target.clone(),
            recover: self.recover.clone(),
            resolve: self.resolve.clone(),
            state: State::Connecting {
                future,
                backoff: None,
            },
        };

        Self::Future { inner: Some(inner) }
    }
}

// === impl ResolveFuture ===

impl<T, E, R> Future for ResolveFuture<T, E, R>
where
    T: fmt::Display + Clone,
    R: resolve::Resolve<T>,
    R::Endpoint: Clone + PartialEq,
    E: Recover,
{
    type Item = Resolution<T, E, R>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = self.inner.as_mut().expect("polled after complete");
        try_ready!(inner.poll_connected());

        Ok(Async::Ready(Resolution {
            inner: self.inner.take().expect("polled after complete"),
            cache: Cache::default(),
        }))
    }
}

// === impl Resolution ===

impl<T, E, R> resolve::Resolution for Resolution<T, E, R>
where
    T: fmt::Display + Clone,
    R: resolve::Resolve<T>,
    R::Endpoint: Clone + PartialEq,
    E: Recover,
{
    type Endpoint = R::Endpoint;
    type Error = Error;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        loop {
            // If a previous reconnect left endpoints to be added, add them
            // immediately.
            if let Some(pending) = self.cache.take_pending() {
                return Ok(pending.into());
            }

            // Ensure that there is an active resolution.
            try_ready!(self.inner.poll_connected());

            let cache = &mut self.cache;
            self.inner.state = match self.inner.state {
                State::Connected {
                    ref mut resolution,
                    ref mut connection,
                } => match resolve::Resolution::poll(resolution) {
                    Ok(ready) => return Ok(ready.map(|u| cache.process_update(u, connection))),
                    Err(e) => State::Failed {
                        error: Some(e.into()),
                        backoff: None,
                    },
                },
                _ => unreachable!("poll_connected must only return ready when connected"),
            };
        }
    }
}

// === impl Inner ===

impl<T, E, R> Inner<T, E, R>
where
    T: fmt::Display + Clone,
    R: resolve::Resolve<T>,
    R::Endpoint: Clone + PartialEq,
    E: Recover,
{
    fn poll_connected(&mut self) -> Poll<(), Error> {
        loop {
            self.state = match self.state {
                State::Disconnected { ref mut backoff } => {
                    tracing::trace!(message = "connecting", target = %self.target);
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
                    Err(e) => State::Failed {
                        error: Some(e.into()),
                        backoff: backoff.take(),
                    },
                    Ok(Async::Ready(resolution)) => State::Connected {
                        resolution,
                        connection: backoff
                            .as_ref()
                            .map(|_| Connection::Reconnected)
                            .unwrap_or(Connection::Established),
                    },
                },

                State::Connected { .. } => {
                    tracing::trace!(message = "connected", target = %self.target);
                    return Ok(Async::Ready(()));
                }

                State::Failed {
                    ref mut error,
                    ref mut backoff,
                } => {
                    let err = error.take().expect("illegal state");
                    tracing::debug!(message = %err, target = %self.target);
                    let new_backoff = self.recover.recover(err)?;
                    State::Backoff(backoff.take().or(Some(new_backoff)))
                }

                State::Backoff(ref mut backoff) => {
                    match backoff
                        .as_mut()
                        .expect("illegal state")
                        .poll()
                        .map_err(Into::into)?
                    {
                        Async::NotReady => return Ok(Async::NotReady),
                        Async::Ready(Some(())) => State::Disconnected {
                            backoff: backoff.take(),
                        },
                        Async::Ready(None) => State::Disconnected { backoff: None },
                    }
                }
            };
        }
    }
}

// === impl Cache ===

impl<T> Default for Cache<T> {
    fn default() -> Self {
        Cache {
            pending_add: IndexMap::default(),
            active: IndexMap::default(),
        }
    }
}

impl<T> Cache<T>
where
    T: Clone + PartialEq,
{
    fn process_update(&mut self, update: Update<T>, connection: &mut Connection) -> Update<T> {
        match update {
            Update::Add(endpoints) => {
                match connection {
                    Connection::Established => {
                        self.active.extend(endpoints.clone());
                        Update::Add(endpoints)
                    }
                    Connection::Reconnected => {
                        *connection = Connection::Established;

                        // Discern which endpoints aren't
                        // actually new, but are being
                        // re-advertised. Also discern which
                        // of the active endpointsshould be
                        // removed.
                        let mut new_endpoints = endpoints.into_iter().collect::<IndexMap<_, _>>();
                        let mut rm_addrs =
                            Vec::with_capacity(self.active.len() - new_endpoints.len());
                        for i in (0..self.active.len()).rev() {
                            let should_remove = {
                                let (addr, endpoint) = self.active.get_index(i).unwrap();
                                match new_endpoints.get(addr) {
                                    None => true,
                                    Some(ep) => {
                                        if *ep == *endpoint {
                                            new_endpoints.remove(addr);
                                        }
                                        false
                                    }
                                }
                            };

                            if should_remove {
                                let (addr, _) = self.active.swap_remove_index(i).unwrap();
                                rm_addrs.push(addr);
                            }
                        }
                        self.pending_add = new_endpoints;
                        Update::Remove(rm_addrs)
                    }
                }
            }
            Update::Remove(addrs) => {
                match connection {
                    Connection::Reconnected => {
                        *connection = Connection::Established;
                        self.active.drain(..);
                    }
                    Connection::Established => {
                        for addr in addrs.iter() {
                            self.active.remove(addr);
                        }
                    }
                }
                Update::Remove(addrs)
            }
            Update::DoesNotExist | Update::Empty => {
                self.active.drain(..);
                update
            }
        }
    }

    fn take_pending(&mut self) -> Option<Update<T>> {
        if self.pending_add.is_empty() {
            return None;
        }

        let endpoints = self.pending_add.drain(..).collect::<Vec<_>>();
        self.active.extend(endpoints.clone());
        Some(Update::Add(endpoints))
    }
}

#![deny(warnings, rust_2018_idioms)]

use futures::{Async, Future, Poll};
use indexmap::IndexMap;
use linkerd2_proxy_core::Error;
use linkerd2_proxy_core::resolve::{self, Update};
use std::fmt;
use std::net::SocketAddr;
use tokio::timer;

mod recover;

pub use self::recover::Recover;

pub struct Resolve<R, E> {
    resolve: R,
    recover: E,
}

pub struct Resolution<T, R: resolve::Resolve<T>, E> {
    target: T,
    resolve: R,
    recover: E,
    active_endpoints: IndexMap<SocketAddr, R::Endpoint>,
    state: State<R::Future, R::Resolution, R::Endpoint>,
}

pub struct ResolveFuture<T, R: resolve::Resolve<T>, E> {
    future: R::Future,
    inner: Option<(T, R, E)>;
}

enum State<F, R, E> {
    Disconnected,
    Connecting(F),
    Connected(Option<R>),
    Reconcile(Option<Update<E>>, Option<R>),
    Resolving(R),
    Failed(Option<Error>),
    Backoff(timer::Delay),
}

impl<T, R, E> tower::Service<T> for Resolution<T, R, E>
where
    T: Clone + fmt::Display,
    R: resolve::Resolve<T>,
    R::Endpoint: Clone + PartialEq,
    E: Recover<Error>,
{
    type Future = ResolveFuture<T, R, E>;


}

impl<T, R, E> resolve::Resolution for Resolution<T, R, E>
where
    T: Clone + fmt::Display,
    R: resolve::Resolve<T>,
    R::Endpoint: Clone + PartialEq,
    E: Recover<Error>,
{
    type Endpoint = R::Endpoint;
    type Error = Error;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        loop {
            self.state = match self.state {
                State::Disconnected => {
                    tracing::trace!(message = "connecting", target = %self.target);
                    let fut = self.resolve.resolve(self.target.clone());
                    State::Connecting(fut)
                }

                State::Connecting(ref mut fut) => match fut.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(e) => State::Failed(Some(e.into())),
                    Ok(Async::Ready(resolution)) => State::Connected(Some(resolution)),
                },

                State::Connected(ref mut resolution) => {
                    tracing::trace!(message = "connected", target = %self.target);
                    // If there is a prior state -- endpoints that have already been created, for instance,
                    // Get the first update after reconnecting, reconcile
                    // against the prior state, and then continue resolving...
                    if self.active_endpoints.is_empty() {
                        self.recover.reset();
                        State::Resolving(resolution.take().expect("illegal state"))
                    } else {
                        match resolution.as_mut().expect("illegal state").poll() {
                            Ok(Async::NotReady) => return Ok(Async::NotReady),
                            Err(e) => State::Failed(Some(e.into())),
                            Ok(Async::Ready(update)) => {
                                State::Reconcile(Some(update), resolution.take())
                            }
                        }
                    }
                }

                State::Reconcile(ref mut update, ref mut resolution) => match update.take() {
                    Some(Update::Add(endpoints)) => {
                        if self.active_endpoints.is_empty() {
                            return Ok(Update::Add(endpoints).into());
                        }

                        for (addr, _) in endpoints.iter() {
                            self.active_endpoints.remove(addr);
                        }

                        // The next time through, there will be no
                        // active_endpoints so the added endpoints will be
                        // returned immediately.
                        let rm_addrs = self.active_endpoints.drain(..).map(|(addr, _)| addr);
                        *update = Some(Update::Add(endpoints));

                        return Ok(Update::Remove(rm_addrs.collect()).into());
                    }
                    Some(Update::Remove(mut addrs)) => {
                        addrs.extend(self.active_endpoints.drain(..).map(|(addr, _)| addr));
                        return Ok(Update::Remove(addrs).into());
                    }
                    Some(dne) => {
                        if self.active_endpoints.is_empty() {
                            return Ok(dne.into());
                        }

                        let addrs = self.active_endpoints.drain(..).map(|(addr, _)| addr);
                        *update = Some(dne);
                        return Ok(Update::Remove(addrs.collect()).into());
                    }
                    None => {
                        self.recover.reset();
                        State::Resolving(resolution.take().expect("illegal state"))
                    }
                },

                State::Resolving(ref mut resolution) => match resolution.poll() {
                    Ok(ready) => return Ok(ready),
                    Err(e) => State::Failed(Some(e.into())),
                },

                State::Failed(ref mut e) => {
                    let err = e.take().expect("illegal staet");
                    tracing::debug!(message = %err, target = %self.target);
                    let delay = self.recover.recover(err)?;
                    State::Backoff(delay)
                }

                State::Backoff(ref mut fut) => match fut.poll().expect("timer must not fail") {
                    Async::NotReady => return Ok(Async::NotReady),
                    Async::Ready(()) => State::Disconnected,
                },
            };
        }
    }
}

impl<T, R, E> Future for ResolveFuture<T, R, E>
where
    R::Item: Resolve<T>,,
    R::Endpoint: fmt::Debug + Clone + PartialEq,
    M: tower::Service<R::Endpoint>,
{
    type Item = Resolution<T, R, E>;
    type Error = R::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let resolution = try_ready!(self.future.poll());
        let make_endpoint = self.make_endpoint.take().expect("polled after ready");
        Ok(Async::Ready(Discover::new(resolution, make_endpoint)))
    }
}

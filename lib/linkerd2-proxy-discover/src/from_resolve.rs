use futures::{try_ready, Async, Future, Poll};
use indexmap::IndexSet;
use linkerd2_proxy_core::resolve::{Resolution, Resolve, Update};
use std::net::SocketAddr;
use tower::discover::Change;

#[derive(Clone, Debug)]
pub struct FromResolve<R> {
    resolve: R,
}

#[derive(Debug)]
pub struct DiscoverFuture<F> {
    future: F,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
pub struct Discover<R: Resolution> {
    resolution: R,
    active: IndexSet<SocketAddr>,
    pending: Vec<Change<SocketAddr, R::Endpoint>>,
}

// === impl FromResolve ===

impl<R> FromResolve<R> {
    pub fn new<T>(resolve: R) -> Self
    where
        R: Resolve<T>,
    {
        Self { resolve }
    }
}

impl<T, R> tower::Service<T> for FromResolve<R>
where
    R: Resolve<T> + Clone,
{
    type Response = Discover<R::Resolution>;
    type Error = R::Error;
    type Future = DiscoverFuture<R::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.resolve.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        Self::Future {
            future: self.resolve.resolve(target),
        }
    }
}

// === impl DiscoverFuture ===

impl<F> Future for DiscoverFuture<F>
where
    F: Future,
    F::Item: Resolution,
{
    type Item = Discover<F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let resolution = try_ready!(self.future.poll());
        Ok(Async::Ready(Discover::new(resolution)))
    }
}

// === impl Discover ===

impl<R: Resolution> Discover<R> {
    pub fn new(resolution: R) -> Self {
        Self {
            resolution,
            active: IndexSet::default(),
            pending: vec![],
        }
    }
}

impl<R: Resolution> tower::discover::Discover for Discover<R> {
    type Key = SocketAddr;
    type Service = R::Endpoint;
    type Error = R::Error;

    fn poll(&mut self) -> Poll<Change<Self::Key, Self::Service>, Self::Error> {
        loop {
            if let Some(change) = self.pending.pop() {
                return Ok(change.into());
            }

            match try_ready!(self.resolution.poll()) {
                Update::Add(endpoints) => {
                    for (addr, endpoint) in endpoints.into_iter() {
                        self.active.insert(addr);
                        self.pending.push(Change::Insert(addr, endpoint));
                    }
                }
                Update::Remove(addrs) => {
                    for addr in addrs.into_iter() {
                        if self.active.remove(&addr) {
                            self.pending.push(Change::Remove(addr));
                        }
                    }
                }
                Update::DoesNotExist | Update::Empty => {
                    self.pending
                        .extend(self.active.drain(..).map(Change::Remove));
                }
            }
        }
    }
}

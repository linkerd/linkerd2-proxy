#![deny(warnings, rust_2018_idioms)]

use self::cache::Cache;
pub use self::layer::Layer;
pub use self::purge::Purge;
use futures::{future, Async, Poll};
use linkerd2_error::Error;
use linkerd2_lock::{Guard, Lock};
use linkerd2_stack::NewService;
use std::hash::Hash;
use std::time::Duration;
use tracing::{debug, trace, warn};

mod cache;
pub mod error;
pub mod layer;
mod purge;

pub struct Service<T, M>
where
    T: Clone + Eq + Hash,
    M: NewService<T>,
{
    make: M,
    cache: Lock<Cache<T, M::Service>>,
    guard: Option<Guard<Cache<T, M::Service>>>,
    _hangup: purge::Handle,
}

// === impl Service ===

impl<T, M> Service<T, M>
where
    T: Clone + Eq + Hash,
    M: NewService<T>,
    M::Service: Clone,
{
    pub fn new(make: M, capacity: usize, max_idle_age: Duration) -> (Self, Purge<T, M::Service>) {
        let cache = Lock::new(Cache::new(capacity, max_idle_age));
        let (purge, _hangup) = Purge::new(cache.clone());
        let router = Self {
            cache,
            make,
            guard: None,
            _hangup,
        };

        (router, purge)
    }
}

impl<T, M> Clone for Service<T, M>
where
    T: Clone + Eq + Hash,
    M: NewService<T> + Clone,
    M::Service: Clone,
{
    fn clone(&self) -> Self {
        Self {
            make: self.make.clone(),
            cache: self.cache.clone(),
            guard: None,
            _hangup: self._hangup.clone(),
        }
    }
}

impl<T, M> tower::Service<T> for Service<T, M>
where
    T: Clone + Eq + Hash,
    M: NewService<T>,
    M::Service: Clone,
{
    type Response = M::Service;
    type Error = Error;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if self.guard.is_none() {
            match self.cache.poll_acquire() {
                Async::NotReady => return Ok(Async::NotReady),
                Async::Ready(guard) => {
                    self.guard = Some(guard);
                }
            }
        }

        debug_assert!(self.guard.is_some());
        Ok(Async::Ready(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let mut cache = self.guard.take().expect("poll_ready must be called");

        if let Some(service) = cache.access(&target) {
            trace!("target already exists in cache");
            return future::ok(service.clone().into());
        }

        let available = cache.available();
        if available == 0 {
            warn!(capacity = %cache.capacity(), "exhausted");
            return future::err(error::NoCapacity(cache.capacity()).into());
        }

        // Make a new service for the target
        let service = self.make.new_service(target.clone());

        debug!(%available, "inserting new target into cache");
        cache.insert(target.clone(), service.clone());

        future::ok(service.into())
    }
}

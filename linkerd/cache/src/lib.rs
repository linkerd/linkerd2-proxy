#![deny(warnings, rust_2018_idioms)]

use futures::{future, Async, Poll};
use linkerd2_error::Never;
use linkerd2_lock::{Guard, Lock};
use linkerd2_stack::NewService;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Weak};
use tracing::{debug, trace};

pub mod layer;

pub use self::layer::CacheLayer;

pub struct Cache<T, N>
where
    T: Eq + Hash,
    N: NewService<(T, Handle)>,
{
    new_service: N,
    lock: Lock<Services<T, N::Service>>,
    guard: Option<Guard<Services<T, N::Service>>>,
}

/// A tracker inserted into each inner service that, when dropped, indicates the service may be
/// removed from the cache.
#[derive(Clone, Debug)]
pub struct Handle(Arc<()>);

type Services<T, S> = HashMap<T, (S, Weak<()>)>;

// === impl Cache ===

impl<T, N> Cache<T, N>
where
    T: Eq + Hash,
    N: NewService<(T, Handle)>,
{
    pub fn new(new_service: N) -> Self {
        Self {
            new_service,
            guard: None,
            lock: Lock::new(Services::default()),
        }
    }
}

impl<T, N> Clone for Cache<T, N>
where
    T: Clone + Eq + Hash,
    N: NewService<(T, Handle)> + Clone,
    N::Service: Clone,
{
    fn clone(&self) -> Self {
        Self {
            new_service: self.new_service.clone(),
            lock: self.lock.clone(),
            guard: None,
        }
    }
}

impl<T, N> tower::Service<T> for Cache<T, N>
where
    T: Clone + Eq + Hash,
    N: NewService<(T, Handle)>,
    N::Service: Clone,
{
    type Response = N::Service;
    type Error = Never;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if self.guard.is_none() {
            match self.lock.poll_acquire() {
                Async::NotReady => return Ok(Async::NotReady),
                Async::Ready(mut services) => {
                    // Drop defunct services before interacting with the cache.
                    let n = services.len();
                    services.retain(|_, (_, weak)| {
                        if weak.upgrade().is_some() {
                            true
                        } else {
                            debug!("Dropping defunct service");
                            false
                        }
                    });
                    trace!(services = services.len(), dropped = n - services.len());

                    self.guard = Some(services);
                }
            }
        }

        debug_assert!(self.guard.is_some(), "guard must be acquired");
        Ok(Async::Ready(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let mut services = self.guard.take().expect("poll_ready must be called");

        if let Some((service, weak)) = services.get(&target) {
            if weak.upgrade().is_some() {
                trace!("Using cached service");
                return future::ok(service.clone());
            }
        }

        // Make a new service for the target
        let handle = Arc::new(());
        let weak = Arc::downgrade(&handle);
        let service = self
            .new_service
            .new_service((target.clone(), Handle(handle)));

        debug!("Caching new service");
        services.insert(target, (service.clone(), weak));

        future::ok(service.into())
    }
}

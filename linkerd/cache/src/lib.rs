#![deny(warnings, rust_2018_idioms)]
use futures::future;
use linkerd2_error::Never;
use linkerd2_stack::NewService;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use tracing::{debug, trace};

pub mod layer;

pub use self::layer::CacheLayer;

pub struct Cache<T, N>
where
    T: Eq + Hash,
    N: NewService<(T, Handle)>,
{
    new_service: N,
    services: Services<T, N::Service>,
}

/// A tracker inserted into each inner service that, when dropped, indicates the service may be
/// removed from the cache.
#[derive(Clone, Debug)]
pub struct Handle(Arc<()>);

type Services<T, S> = HashMap<T, (S, Weak<()>)>;

// === impl Cache ===

impl<T, N> Cache<T, N>
where
    T: Eq + Hash + Send,
    N: NewService<(T, Handle)>,
{
    pub fn new(new_service: N) -> Self {
        Self {
            new_service,
            services: Services::default(),
        }
    }
}

impl<T, N> tower::Service<T> for Cache<T, N>
where
    T: Clone + Eq + Hash + Send,
    N: NewService<(T, Handle)>,
    N::Service: Clone + Send,
{
    type Response = N::Service;
    type Error = Never;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        if let Some((service, weak)) = self.services.get(&target) {
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

        // Drop defunct services before inserting the new service into the
        // cache.
        let n = self.services.len();
        self.services.retain(|_, (_, weak)| {
            if weak.strong_count() > 0 {
                true
            } else {
                trace!("Dropping defunct service");
                false
            }
        });
        debug!(
            services = self.services.len(),
            dropped = n - self.services.len()
        );

        debug!("Caching new service");
        self.services.insert(target, (service.clone(), weak));

        future::ok(service.into())
    }
}

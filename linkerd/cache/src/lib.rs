#![deny(warnings, rust_2018_idioms)]

use linkerd2_stack::NewService;
use parking_lot::RwLock;
use std::{
    collections::{hash_map::Entry, HashMap},
    hash::Hash,
    sync::{Arc, Weak},
};
use tracing::{debug, trace};

pub mod layer;

pub use self::layer::CacheLayer;

#[derive(Clone)]
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

type Services<T, S> = Arc<RwLock<HashMap<T, (S, Weak<()>)>>>;

// === impl Cache ===

impl<T, N> Cache<T, N>
where
    T: Eq + Hash,
    N: NewService<(T, Handle)>,
{
    pub fn new(new_service: N) -> Self {
        Self {
            new_service,
            services: Services::default(),
        }
    }

    fn new_entry(new: &mut N, target: T) -> (N::Service, Weak<()>) {
        let handle = Arc::new(());
        let weak = Arc::downgrade(&handle);
        let svc = new.new_service((target, Handle(handle)));
        (svc, weak)
    }
}

impl<T, N> NewService<T> for Cache<T, N>
where
    T: Clone + Eq + Hash,
    N: NewService<(T, Handle)>,
    N::Service: Clone,
{
    type Service = N::Service;

    fn new_service(&mut self, target: T) -> N::Service {
        // We expect the item to be available in most cases, so initially obtain only a read lock.
        if let Some((service, weak)) = self.services.read().get(&target) {
            if weak.upgrade().is_some() {
                trace!("Using cached service");
                return service.clone();
            }
        }

        // Otherwise, obtain a write lock to insert a new service.
        let mut services = self.services.write();

        let service = match services.entry(target.clone()) {
            Entry::Occupied(mut entry) => {
                // Another thread raced us to create a service for this target. Use it.
                let (svc, weak) = entry.get();
                if weak.upgrade().is_some() {
                    trace!("Using cached service");
                    svc.clone()
                } else {
                    debug!("Caching new service");
                    let (svc, weak) = Self::new_entry(&mut self.new_service, target);
                    entry.insert((svc.clone(), weak));
                    svc
                }
            }
            Entry::Vacant(entry) => {
                // Make a new service for the target.
                debug!("Caching new service");
                let (svc, weak) = Self::new_entry(&mut self.new_service, target);
                entry.insert((svc.clone(), weak));
                svc
            }
        };

        // Drop defunct services before inserting the new service into the
        // cache.
        let n = services.len();
        services.retain(|_, (_, weak)| {
            if weak.strong_count() > 0 {
                true
            } else {
                trace!("Dropping defunct service");
                false
            }
        });
        debug!(services = services.len(), dropped = n - services.len());

        service
    }
}

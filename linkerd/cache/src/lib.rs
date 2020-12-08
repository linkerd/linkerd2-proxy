#![deny(warnings, rust_2018_idioms)]

pub mod track;

pub use self::track::{NewTrack, Track};
use linkerd2_stack::{layer, NewService};
use parking_lot::RwLock;
use std::{
    collections::{hash_map::Entry, HashMap},
    hash::Hash,
    sync::{Arc, Weak},
};
use tracing::{debug, trace};

#[derive(Clone)]
pub struct Cache<T, N, S>
where
    T: Eq + Hash,
{
    inner: N,
    services: Services<T, S>,
}

/// A tracker inserted into each inner service that, when dropped, indicates the
/// service may be removed from the cache.
pub type Handle = Arc<()>;

type Services<T, S> = Arc<RwLock<HashMap<T, (S, Weak<()>)>>>;

// === impl Cache ===

impl<T, N> Cache<T, N, N::Service>
where
    T: Eq + Hash,
    N: NewService<(Handle, T)>,
{
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Copy + Clone {
        layer::mk(|inner| Self {
            inner,
            services: Services::default(),
        })
    }

    fn new_entry(new: &mut N, target: T) -> (N::Service, Weak<()>) {
        let handle = Arc::new(());
        let weak = Arc::downgrade(&handle);
        let svc = new.new_service((handle, target));
        (svc, weak)
    }
}

impl<T, N> NewService<T> for Cache<T, N, N::Service>
where
    T: Clone + Eq + Hash,
    N: NewService<(Handle, T)>,
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
                    debug!("Replacing defunct service");
                    let (svc, weak) = Self::new_entry(&mut self.inner, target);
                    entry.insert((svc.clone(), weak));
                    svc
                }
            }
            Entry::Vacant(entry) => {
                // Make a new service for the target.
                debug!("Caching new service");
                let (svc, weak) = Self::new_entry(&mut self.inner, target);
                entry.insert((svc.clone(), weak));
                svc
            }
        };

        // Drop defunct services before inserting the new service into the
        // cache.
        let orig_len = services.len();
        services.retain(|_, (_, weak)| {
            if weak.strong_count() > 0 {
                true
            } else {
                trace!("Dropping defunct service");
                false
            }
        });
        debug!(
            services = services.len(),
            dropped = orig_len - services.len()
        );

        service
    }
}

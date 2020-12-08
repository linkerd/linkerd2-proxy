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
use tokio::sync::Notify;
use tracing::{debug, debug_span, trace};
use tracing_futures::Instrument;

#[derive(Clone)]
pub struct Cache<T, N, S>
where
    T: Eq + Hash,
{
    inner: N,
    services: Services<T, S>,

    // As long as this is held, the eviction tasks will activate every time it is
    // notified.
    evict: Arc<Notify>,
}

/// A tracker inserted into each inner service that, when dropped, indicates the
/// service may be removed from the cache.
#[derive(Debug)]
pub struct Handle {
    inner: Arc<()>,
    evict: Weak<Notify>,
}

type Services<T, S> = Arc<RwLock<HashMap<T, (S, Weak<()>)>>>;

// === impl Handle ===

impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(evict) = self.evict.upgrade() {
            evict.notify_one();
        }
    }
}

// === impl Cache ===

impl<T, N> Cache<T, N, N::Service>
where
    T: Eq + Hash + Send + Sync + 'static,
    N: NewService<(Handle, T)> + 'static,
    N::Service: Send + Sync + 'static,
{
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Copy + Clone {
        layer::mk(|inner| {
            let (cache, _) = Self::spawn(inner);
            cache
        })
    }

    fn spawn(inner: N) -> (Self, tokio::task::JoinHandle<()>) {
        let services = Services::default();
        let evict = Arc::new(Notify::new());

        // Spawn a background task to remove defunct services.
        let task = tokio::spawn(
            Self::evict(Arc::downgrade(&evict), services.clone()).instrument(debug_span!("evict")),
        );

        let cache = Self {
            inner,
            services,
            evict,
        };
        (cache, task)
    }

    // Evicts services every time the evict handle is notified, as long as the
    // evict handle remains active.
    async fn evict(evict: Weak<Notify>, services: Services<T, N::Service>) {
        loop {
            // If the cache still holds the eviction handle, wait for it to be
            // notified before evicting services. It is notified whenever a
            // handle is dropped and when the cache is dropped.
            match evict.upgrade() {
                Some(e) => {
                    trace!("Awaiting eviction signal");
                    e.notified().await;
                    trace!("Evicting services");
                }
                None => {
                    debug!("Cache dropped");
                    return;
                }
            }

            // Drop defunct services before inserting the new service into the
            // cache.
            let mut svcs = services.write();
            let orig_len = svcs.len();
            svcs.retain(|_, (_, weak)| {
                if weak.strong_count() > 0 {
                    true
                } else {
                    trace!("Dropping defunct service");
                    false
                }
            });
            debug!(services = svcs.len(), dropped = orig_len - svcs.len());
        }
    }
}

impl<T: Eq + Hash, N, S> Drop for Cache<T, N, S> {
    fn drop(&mut self) {
        // When the cache is dropped, notify the eviction task so it terminates.
        self.evict.notify_one()
    }
}

impl<T, N> Cache<T, N, N::Service>
where
    T: Eq + Hash,
    N: NewService<(Handle, T)>,
{
    fn new_entry(new: &mut N, target: T, evict: &Arc<Notify>) -> (N::Service, Weak<()>) {
        let inner = Arc::new(());
        let weak = Arc::downgrade(&inner);
        let evict = Arc::downgrade(evict);
        let svc = new.new_service((Handle { inner, evict }, target));
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
        // We expect the item to be available in most cases, so initially obtain
        // only a read lock.
        if let Some((service, weak)) = self.services.read().get(&target) {
            if weak.upgrade().is_some() {
                trace!("Using cached service");
                return service.clone();
            }
        }

        // Otherwise, obtain a write lock to insert a new service.
        match self.services.write().entry(target.clone()) {
            Entry::Occupied(mut entry) => {
                // Another thread raced us to create a service for this target.
                // Try to use it.
                let (svc, weak) = entry.get();
                if weak.upgrade().is_some() {
                    trace!("Using cached service");
                    svc.clone()
                } else {
                    debug!("Replacing defunct service");
                    let (svc, weak) = Self::new_entry(&mut self.inner, target, &self.evict);
                    entry.insert((svc.clone(), weak));
                    svc
                }
            }
            Entry::Vacant(entry) => {
                debug!("Caching new service");
                let (svc, weak) = Self::new_entry(&mut self.inner, target, &self.evict);
                entry.insert((svc.clone(), weak));
                svc
            }
        }
    }
}

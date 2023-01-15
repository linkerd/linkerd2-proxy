use super::*;
use linkerd_stack::{layer, NewService, Param};

#[derive(Clone)]
pub struct NewIdleCached<K, N, S>
where
    K: Eq + Hash,
{
    cache: IdleCache<K, S>,
    new_svc: N,
}

// === impl NewIdleCached ===

impl<K, N, S> NewIdleCached<K, N, S>
where
    K: Clone + std::fmt::Debug + Eq + Hash + Send + Sync + 'static,
    N: 'static,
    S: Send + Sync + 'static,
{
    pub fn layer(idle: time::Duration) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new_svc| Self {
            new_svc,
            cache: IdleCache::new(idle),
        })
    }
}

impl<T, K, N> NewService<T> for NewIdleCached<K, N, N::Service>
where
    T: Param<K>,
    K: Clone + std::fmt::Debug + Eq + Hash + Send + Sync + 'static,
    N: NewService<T> + 'static,
    N::Service: Clone + Send + Sync + 'static,
{
    type Service = Cached<N::Service>;

    fn new_service(&self, target: T) -> Cached<N::Service> {
        self.cache
            .get_or_insert_with(target.param(), |_| self.new_svc.new_service(target))
    }
}

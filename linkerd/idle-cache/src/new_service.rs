use super::*;
use linkerd_stack::{layer, NewService};

#[derive(Clone)]
pub struct NewIdleCached<T, N>
where
    T: Eq + Hash,
    N: NewService<T>,
{
    cache: IdleCache<T, N::Service>,
    new_svc: N,
}

// === impl NewIdleCached ===

impl<T, N> NewIdleCached<T, N>
where
    T: Clone + std::fmt::Debug + Eq + Hash + Send + Sync + 'static,
    N: NewService<T> + 'static,
    N::Service: Send + Sync + 'static,
{
    pub fn layer(idle: time::Duration) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new_svc| Self {
            new_svc,
            cache: IdleCache::new(idle),
        })
    }
}

impl<T, N> NewService<T> for NewIdleCached<T, N>
where
    T: Clone + std::fmt::Debug + Eq + Hash + Send + Sync + 'static,
    N: NewService<T> + 'static,
    N::Service: Clone + Send + Sync + 'static,
{
    type Service = Cached<N::Service>;

    fn new_service(&self, target: T) -> Cached<N::Service> {
        self.cache
            .get_or_insert_with(target, |target| self.new_svc.new_service(target.clone()))
    }
}
